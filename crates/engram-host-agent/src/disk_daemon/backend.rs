//! `ChunkedDiskBackend` — data plane for the NBD daemon.
//!
//! Maps NBD `(offset, length)` operations onto chunk-store
//! operations. Per-fault read resolves to one chunk fetch (via the
//! L1 NVMe cache); per-write copies the base chunk into a
//! per-session sparse dirty file and applies the write there.
//!
//! Two key invariants the runtime depends on:
//!
//! 1. **Read-after-write** within a session sees the dirty bytes.
//!    Writes don't go back to the chunk store on every NBD_CMD_WRITE
//!    — that would explode object-storage cost. Dirty chunks live
//!    in a sparse file until `flush()` uploads them.
//! 2. **Base chunks are immutable.** The chunk store is content-
//!    addressed; the daemon never PUTs an existing hash again.
//!    Dirty chunks get rehashed at flush time; new hashes go up,
//!    the manifest version ticks.
//!
//! The file follows guest offsets. The in-memory dirty tier keeps
//! only chunk indexes and generations.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
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

static DIRTY_FILE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    /// The per-sandbox dirty file could not be opened, read, written,
    /// synced, scanned, moved, or punched.
    DirtyFile {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    /// NBD offset + length lands past the end of the virtual disk.
    /// The kernel side shouldn't normally send these; if it does,
    /// the daemon replies EINVAL.
    OutOfRange {
        offset: u64,
        length: u64,
        total: u64,
    },
    /// A spool chunk failed the adoption shape check (past `total_bytes`
    /// or wider than `chunk_size`). The adoption is refused ATOMICALLY —
    /// adopting the well-shaped subset would silently roll back the
    /// out-of-shape chunk's acked write (issue #810: the 2026-07-20 roll
    /// served rolled-back base exactly this way, caught only by
    /// verify-on-read). The caller parks the survivor; the spool stays
    /// on disk for diagnosis/retry.
    AdoptShape { chunk_idx: usize, len: usize },
    /// A resolver-fetched chunk's byte length does not match the manifest
    /// slice width — serving it would read out of bounds. Hash-valid but
    /// short/long blobs (manifest corruption, a bad flush) land here as a
    /// typed EIO instead of a slice panic in the NBD daemon.
    ShortChunk {
        chunk_idx: usize,
        expected: u64,
        actual: usize,
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
            Self::DirtyFile {
                operation,
                path,
                source,
            } => write!(f, "dirty file {operation} at {}: {source}", path.display()),
            Self::OutOfRange {
                offset,
                length,
                total,
            } => write!(f, "NBD range {offset}+{length} exceeds total_bytes {total}"),
            Self::ShortChunk {
                chunk_idx,
                expected,
                actual,
            } => write!(
                f,
                "chunk {chunk_idx} length {actual} does not match manifest width {expected}"
            ),
            Self::AdoptShape { chunk_idx, len } => write!(
                f,
                "spool adoption refused: chunk {chunk_idx} (len {len}) is out of shape for this \
                 backend — adopting a partial spool would silently roll back acked writes \
                 (the 85e0298a class); the whole spool is rejected and preserved on disk"
            ),
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

/// ADR 0038 B3 handoff from `flush_local` to `flush_upload`.
/// `new_chunks` is the exact disk state captured while `flush_local`
/// holds the dirty-tier lock. Each claim records the generation for
/// later release or hole punching. Empty `claims` means nothing was
/// dirty.
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
    claims: Vec<DirtyChunkClaim>,
    /// Held across the publish; see `ChunkedDiskBackend::flush_pipeline`.
    flush_guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl PendingDiskFlush {
    #[cfg(target_os = "linux")]
    pub(crate) fn chunks(&self) -> &[(usize, ChunkHash, Bytes)] {
        &self.new_chunks
    }

    /// Return the bytes captured at the pause instant for eviction.
    /// Consumed claims stay in the tier's claimed map until sandbox
    /// destroy, so `unflushed_bytes()` keeps counting them. Reads
    /// continue to use the file because `contains()` includes claims.
    #[cfg(target_os = "linux")]
    pub(crate) fn into_chunks(self) -> Vec<(usize, ChunkHash, Bytes)> {
        self.new_chunks
    }
}

#[derive(Clone, Copy, Debug)]
struct DirtyChunkClaim {
    chunk_idx: usize,
    generation: u64,
}

/// ADR 0045 C2 disk post-copy: the frozen source's sealed disk state
/// — every chunk whose guest-visible content differs from the
/// published base manifest (dirty and claimed file chunks),
/// held as raw refcounted bytes. Never hashed, never uploaded; served
/// by index over `MigrationFetch::DiskChunkAt` and dropped when the
/// export retires (commit) or re-queued into the dirty set (abort).
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

/// Controls whether construction starts a new dirty file or recovers
/// the allocated extents of an existing file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirtyFileOpenMode {
    Truncate,
    Recover,
}

/// File-backed guest divergence for one sandbox.
///
/// One async mutex protects this whole struct. Reads, writes, claims,
/// requeues, and hole punches use this lock. No caller can observe a
/// chunk between tier states, and a writer always bumps the generation
/// before it releases the lock.
struct DirtyFileTier {
    file: File,
    path: PathBuf,
    /// Chunks that the next flush can claim.
    dirty: HashSet<usize>,
    /// Chunks owned by the active flush and their claim generations.
    claimed: HashMap<usize, u64>,
    /// The latest write generation for every chunk touched in this process.
    generations: HashMap<usize, u64>,
    fsync_on_write: bool,
    /// Temporary constructor files are removed if attach fails.
    remove_on_drop: bool,
}

impl DirtyFileTier {
    fn open(
        path: PathBuf,
        total_bytes: u64,
        chunk_size: u64,
        mode: DirtyFileOpenMode,
    ) -> Result<Self, DiskBackendError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| dirty_file_error("create parent", &path, source))?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        match mode {
            DirtyFileOpenMode::Truncate => {
                options.create(true).truncate(true);
            }
            DirtyFileOpenMode::Recover => {}
        }
        let file = options
            .open(&path)
            .map_err(|source| dirty_file_error("open", &path, source))?;
        file.set_len(total_bytes)
            .map_err(|source| dirty_file_error("set length", &path, source))?;
        let mut tier = Self {
            file,
            path,
            dirty: HashSet::new(),
            claimed: HashMap::new(),
            generations: HashMap::new(),
            fsync_on_write: std::env::var("ENGRAM_DIRTY_FSYNC").as_deref() == Ok("1"),
            remove_on_drop: mode == DirtyFileOpenMode::Truncate,
        };
        if mode == DirtyFileOpenMode::Recover {
            tier.recover_extents(total_bytes, chunk_size)?;
        }
        Ok(tier)
    }

    /// Rebuild the dirty set from the file's allocated extents.
    ///
    /// This scan needs a filesystem that reports exact extents, which
    /// ext4 does. Production recovery runs only on Linux hosts. APFS
    /// reports flushed zero-fill as data, so a scan there would mark
    /// clean chunks dirty and serve zeros where base bytes belong.
    fn recover_extents(
        &mut self,
        total_bytes: u64,
        chunk_size: u64,
    ) -> Result<(), DiskBackendError> {
        let fd = self.file.as_raw_fd();
        let mut offset = 0u64;
        while offset < total_bytes {
            // SAFETY: lseek reads extent metadata from a file descriptor
            // that this tier owns. It does not access process memory.
            let data = unsafe { libc::lseek(fd, offset as libc::off_t, libc::SEEK_DATA) };
            if data < 0 {
                let source = std::io::Error::last_os_error();
                if source.raw_os_error() == Some(libc::ENXIO) {
                    break;
                }
                return Err(dirty_file_error("scan data extent", &self.path, source));
            }
            // SAFETY: this uses the same owned descriptor and only reads
            // filesystem extent metadata.
            let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
            if hole < 0 {
                return Err(dirty_file_error(
                    "scan hole extent",
                    &self.path,
                    std::io::Error::last_os_error(),
                ));
            }
            let data = data as u64;
            let hole = (hole as u64).min(total_bytes);
            if hole > data {
                let first = (data / chunk_size) as usize;
                let last = ((hole - 1) / chunk_size) as usize;
                for chunk_idx in first..=last {
                    self.dirty.insert(chunk_idx);
                    self.generations.insert(chunk_idx, 1);
                }
            }
            offset = hole;
        }
        Ok(())
    }

    fn contains(&self, chunk_idx: usize) -> bool {
        self.dirty.contains(&chunk_idx) || self.claimed.contains_key(&chunk_idx)
    }

    fn read_chunk(
        &self,
        chunk_idx: usize,
        chunk_len: usize,
        chunk_size: u64,
    ) -> std::io::Result<Bytes> {
        let mut bytes = vec![0u8; chunk_len];
        read_exact_at(&self.file, &mut bytes, chunk_idx as u64 * chunk_size)?;
        Ok(Bytes::from(bytes))
    }

    fn write_chunk(&self, chunk_idx: usize, bytes: &[u8], chunk_size: u64) -> std::io::Result<()> {
        write_all_at(&self.file, bytes, chunk_idx as u64 * chunk_size)
    }

    fn bump_generation(&mut self, chunk_idx: usize) -> Result<u64, DiskBackendError> {
        let generation = self.generations.entry(chunk_idx).or_default();
        *generation = generation.checked_add(1).ok_or_else(|| {
            DiskBackendError::InvariantViolation(format!(
                "dirty generation overflow for chunk {chunk_idx}"
            ))
        })?;
        Ok(*generation)
    }

    fn release_claims(&mut self, claims: &[DirtyChunkClaim]) {
        for claim in claims {
            if self.claimed.get(&claim.chunk_idx) == Some(&claim.generation) {
                self.claimed.remove(&claim.chunk_idx);
                self.dirty.insert(claim.chunk_idx);
            }
        }
    }

    fn allocated_chunks(&self) -> HashSet<usize> {
        self.dirty
            .iter()
            .copied()
            .chain(self.claimed.keys().copied())
            .collect()
    }
}

impl Drop for DirtyFileTier {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn dirty_file_error(
    operation: &'static str,
    path: &Path,
    source: std::io::Error,
) -> DiskBackendError {
    DiskBackendError::DirtyFile {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn read_exact_at(file: &File, mut bytes: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let read = file.read_at(bytes, offset)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "dirty file ended before the requested chunk",
            ));
        }
        offset += read as u64;
        bytes = &mut bytes[read..];
    }
    Ok(())
}

fn write_all_at(file: &File, mut bytes: &[u8], mut offset: u64) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let written = file.write_at(bytes, offset)?;
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "dirty file write made no progress",
            ));
        }
        offset += written as u64;
        bytes = &bytes[written..];
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn punch_hole(file: &File, offset: u64, length: u64) -> std::io::Result<()> {
    // SAFETY: fallocate changes allocation metadata for an owned file
    // descriptor. The offset and length are within the file size.
    let result = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            offset as libc::off_t,
            length as libc::off_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn punch_hole(file: &File, offset: u64, length: u64) -> std::io::Result<()> {
    let mut request = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: offset as libc::off_t,
        fp_length: length as libc::off_t,
    };
    // SAFETY: fcntl reads the initialized request and changes allocation
    // metadata for an owned file descriptor.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &mut request) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
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
    /// The sparse dirty file and its chunk metadata. The lock is the
    /// only dirty-tier lock. A claimed chunk stays readable from this
    /// file until the manifest is rebased and cleanup punches its hole.
    dirty_tier: Arc<Mutex<DirtyFileTier>>,
    /// Unix-millis timestamp of the last successful `flush()`
    /// completion. `0` = never flushed since construction (the
    /// sentinel the diagnostic surface renders as "never").
    /// Read lock-free for the COW state RPC; written at the tail of
    /// `flush()` after the new manifest is durably published.
    last_flush_unix_ms: Arc<AtomicI64>,
    /// ADR 0016 Phase B: when dirty bytes monotonically cross
    /// `threshold_bytes` inside the dirty-tier lock, the writer pokes
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
    /// (`flush_local` claim → `flush_upload` GCS put → manifest
    /// rebuild/publish → `base` rebase) so two flushes can never run
    /// their upload/rebase phases concurrently. Without this only the
    /// dirty-set claim was serialized, and whichever flush rebased LAST
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
    /// before `dirty_tier` in `flush_local` and before `state` in
    /// `flush_upload`. The per-sandbox
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

    /// ADR 0098 D1: the fork-manifest identity is minted through injected
    /// entropy (it becomes the NBD `backend_identifier` — a decision id).
    /// P1 wires the production `OsEntropy`.
    entropy: Arc<dyn engram_core::traits::Entropy>,

    /// ADR 0098 P6 (Flow F): the flush pipeline's 3-point scheduler seam —
    /// the generalization of the old test-only #204 handoff barrier. An
    /// armed seam parks the pipeline at exactly one [`FlushSeamPoint`]
    /// (signal `arrived`, await `proceed`) so a test or the host-internal
    /// simulator can interleave reads/writes/fence-raises/crashes against
    /// the documented flush hazards (#204 tier-less window, #199 fence +
    /// publish ordering, the pre-rebase store-ahead crash window).
    ///
    /// Deliberately NOT `#[cfg(test)]`: `engram-dst-host` (a separate
    /// crate) drives it. The prod cost is one uncontended mutex check per
    /// flush STAGE on a ~30 s flush cadence — the data-plane per-op paths
    /// (read/write/serve) never touch it. `None` in every prod build.
    scheduler_seam: std::sync::Mutex<Option<ArmedFlushSeam>>,
}

/// The three flush-pipeline instants an armed [`ChunkedDiskBackend`]
/// scheduler seam can park at (ADR 0098 P6, Flow F).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushSeamPoint {
    /// Inside `flush_local`: the drained chunks have moved into the held
    /// `pending` map, BOTH locks still held — exactly the instant the
    /// pre-fix #204 code left a chunk in NO tier.
    DirtyPendingHandoff,
    /// Inside `flush_upload`: every GCS put is durable, BEFORE the state
    /// lock, the #199 migration-fence re-check, and the manifest publish
    /// — the window a fence raised mid-upload must still abort.
    PostUploadPrePublish,
    /// Inside `flush_upload`: the manifest is PUBLISHED (`put_manifest`
    /// succeeded), BEFORE the base/state rebase and the pending-tier
    /// drop — a crash here leaves the store AHEAD of every consumer (the
    /// 85e0298a store-ahead shape; recovery is the version-conflict
    /// retry).
    PreRebase,
}

/// The two-phase handshake an armed seam point carries: the pipeline
/// signals `arrived` at the point (relevant locks held), then awaits
/// `proceed`.
struct ArmedFlushSeam {
    point: FlushSeamPoint,
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

fn default_dirty_file_path(cache: &ChunkCache, manifest_ref: ManifestRef) -> PathBuf {
    let sequence = DIRTY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    cache.root().join("dirty").join(format!(
        ".backend-{}-{}-{}-{sequence}.cache",
        manifest_ref.manifest_id,
        manifest_ref.version,
        std::process::id()
    ))
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
        let dirty_path = default_dirty_file_path(&cache, manifest_ref);
        Self::from_blob_with_dirty_file(
            manifest_ref,
            cache,
            store,
            threshold_bytes,
            dirty_path,
            DirtyFileOpenMode::Truncate,
        )
        .await
    }

    pub(crate) async fn from_blob_with_dirty_file(
        manifest_ref: ManifestRef,
        cache: ChunkCache,
        store: Arc<ChunkStore>,
        threshold_bytes: u64,
        dirty_path: PathBuf,
        dirty_mode: DirtyFileOpenMode,
    ) -> Result<Self, DiskBackendError> {
        let manifest = store.get_manifest(manifest_ref).await?;
        Self::from_manifest_with_dirty_file(
            manifest_ref,
            &manifest,
            cache,
            store,
            threshold_bytes,
            dirty_path,
            dirty_mode,
        )
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
        let dirty_path = default_dirty_file_path(&cache, manifest_ref);
        Self::from_manifest_with_dirty_file(
            manifest_ref,
            manifest,
            cache,
            store,
            threshold_bytes,
            dirty_path,
            DirtyFileOpenMode::Truncate,
        )
    }

    pub(crate) fn from_manifest_with_dirty_file(
        manifest_ref: ManifestRef,
        manifest: &engram_chunk_store::Manifest,
        cache: ChunkCache,
        store: Arc<ChunkStore>,
        threshold_bytes: u64,
        dirty_path: PathBuf,
        dirty_mode: DirtyFileOpenMode,
    ) -> Result<Self, DiskBackendError> {
        let base = PositionalDiskManifest::from_manifest(manifest)?;
        let chunk_size = base.chunk_size;
        let total_bytes = base.total_bytes;
        let dirty_tier = DirtyFileTier::open(dirty_path, total_bytes, chunk_size, dirty_mode)?;
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
            dirty_tier: Arc::new(Mutex::new(dirty_tier)),
            last_flush_unix_ms: Arc::new(AtomicI64::new(0)),
            threshold_notify: Arc::new(Notify::new()),
            migration_fence: std::sync::atomic::AtomicBool::new(false),
            flush_pipeline: Arc::new(tokio::sync::Mutex::new(())),
            post_copy: Arc::new(std::sync::Mutex::new(None)),
            threshold_bytes,
            in_flight: Arc::new(InFlightTracker::new()),
            operation_scope: crate::trace_scope::OperationScope::default(),
            entropy: Arc::new(engram_core::traits::OsEntropy),
            scheduler_seam: std::sync::Mutex::new(None),
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
        Self::from_manifest(manifest_ref, manifest, cache, store, threshold_bytes)
    }

    /// Move a newly-created temporary file to its stable sandbox path.
    /// The open descriptor stays valid across the rename.
    #[cfg(target_os = "linux")]
    pub(crate) async fn relocate_dirty_file(&self, target: &Path) -> Result<(), DiskBackendError> {
        let mut tier = self.dirty_tier.lock().await;
        if tier.path == target {
            tier.remove_on_drop = false;
            return Ok(());
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| dirty_file_error("create parent", target, source))?;
        }
        if target
            .try_exists()
            .map_err(|source| dirty_file_error("check destination", target, source))?
        {
            return Err(dirty_file_error(
                "rename",
                target,
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "sandbox dirty file already exists",
                ),
            ));
        }
        std::fs::rename(&tier.path, target)
            .map_err(|source| dirty_file_error("rename", target, source))?;
        tier.path = target.to_path_buf();
        tier.remove_on_drop = false;
        Ok(())
    }

    /// Keep an explicitly named stable file when the backend drops.
    #[cfg(target_os = "linux")]
    pub(crate) async fn retain_dirty_file(&self) {
        self.dirty_tier.lock().await.remove_on_drop = false;
    }

    fn chunk_len(&self, chunk_idx: usize) -> u64 {
        let start = chunk_idx as u64 * self.chunk_size;
        self.chunk_size.min(self.total_bytes.saturating_sub(start))
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
    /// flush, which is correct: one flush claims the full dirty set.
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

    /// Number of chunks waiting for the next flush claim.
    pub async fn dirty_chunks_count(&self) -> usize {
        self.dirty_tier.lock().await.dirty.len()
    }

    /// Whole-chunk bytes waiting for the next flush claim.
    pub async fn dirty_bytes(&self) -> u64 {
        let tier = self.dirty_tier.lock().await;
        tier.dirty.iter().map(|idx| self.chunk_len(*idx)).sum()
    }

    /// Whole-chunk bytes that are dirty or owned by an active claim.
    pub async fn unflushed_bytes(&self) -> u64 {
        let tier = self.dirty_tier.lock().await;
        tier.allocated_chunks()
            .iter()
            .map(|idx| self.chunk_len(*idx))
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
    /// boundaries; each chunk read serves from the dirty file
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
        for (&(chunk_idx, read_len, intra, take), chunk_bytes) in
            descriptors.iter().zip(fetched.iter())
        {
            let slice = chunk_bytes.get(intra..intra + take).ok_or_else(|| {
                DiskBackendError::ShortChunk {
                    chunk_idx,
                    expected: read_len,
                    actual: chunk_bytes.len(),
                }
            })?;
            out.extend_from_slice(slice);
        }
        Ok(Bytes::from(out))
    }

    /// Write `data` at `offset`. Idempotent on the same byte range
    /// — last writer wins. Materialises the affected chunks into
    /// the dirty file on first touch (copy from base, then patch).
    /// File writes and generation changes share one tier critical
    /// section, so a flush claim cannot split a guest write.
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
        let tier = self.dirty_tier.lock().await;
        if tier.fsync_on_write {
            tier.file
                .sync_data()
                .map_err(|source| dirty_file_error("sync write", &tier.path, source))?;
        }
        Ok(())
    }

    /// Patch one chunk in the dirty file. A first write materializes
    /// the full visible chunk before it applies the guest bytes.
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
        loop {
            let observed_generation = {
                let mut tier = self.dirty_tier.lock().await;
                if tier.contains(chunk_idx) {
                    let before: u64 = tier.dirty.iter().map(|idx| self.chunk_len(*idx)).sum();
                    write_all_at(
                        &tier.file,
                        payload,
                        chunk_idx as u64 * self.chunk_size + intra as u64,
                    )
                    .map_err(|source| dirty_file_error("write guest bytes", &tier.path, source))?;
                    tier.bump_generation(chunk_idx)?;
                    tier.dirty.insert(chunk_idx);
                    let after: u64 = tier.dirty.iter().map(|idx| self.chunk_len(*idx)).sum();
                    drop(tier);
                    if before < self.threshold_bytes && after >= self.threshold_bytes {
                        self.threshold_notify.notify_one();
                    }
                    return Ok(());
                }
                tier.generations.get(&chunk_idx).copied().unwrap_or(0)
            };

            // Fetch without the tier lock. The persistent generation lets
            // us detect a write and flush that completed during this fetch.
            let base = self.read_chunk(chunk_idx, chunk_len as u64).await?;
            let mut tier = self.dirty_tier.lock().await;
            if tier.contains(chunk_idx) {
                let before: u64 = tier.dirty.iter().map(|idx| self.chunk_len(*idx)).sum();
                write_all_at(
                    &tier.file,
                    payload,
                    chunk_idx as u64 * self.chunk_size + intra as u64,
                )
                .map_err(|source| dirty_file_error("write guest bytes", &tier.path, source))?;
                tier.bump_generation(chunk_idx)?;
                tier.dirty.insert(chunk_idx);
                let after: u64 = tier.dirty.iter().map(|idx| self.chunk_len(*idx)).sum();
                drop(tier);
                if before < self.threshold_bytes && after >= self.threshold_bytes {
                    self.threshold_notify.notify_one();
                }
                return Ok(());
            }
            if tier.generations.get(&chunk_idx).copied().unwrap_or(0) != observed_generation {
                drop(tier);
                continue;
            }
            let before: u64 = tier.dirty.iter().map(|idx| self.chunk_len(*idx)).sum();
            tier.write_chunk(chunk_idx, &base, self.chunk_size)
                .map_err(|source| dirty_file_error("materialize chunk", &tier.path, source))?;
            write_all_at(
                &tier.file,
                payload,
                chunk_idx as u64 * self.chunk_size + intra as u64,
            )
            .map_err(|source| dirty_file_error("write guest bytes", &tier.path, source))?;
            tier.bump_generation(chunk_idx)?;
            tier.dirty.insert(chunk_idx);
            let after: u64 = tier.dirty.iter().map(|idx| self.chunk_len(*idx)).sum();
            drop(tier);
            if before < self.threshold_bytes && after >= self.threshold_bytes {
                self.threshold_notify.notify_one();
            }
            return Ok(());
        }
    }

    /// Shutdown-spool export (2026-07-16 session-85e0298a RCA): a
    /// coherent snapshot of every un-uploaded chunk — dirty or claimed — and
    /// the manifest ref they diverge from. Intended to run AFTER the
    /// serve loop is dead (SIGTERM abandon), when the tiers are frozen.
    ///
    /// The tier snapshot comes before the manifest ref. A chunk cleaned
    /// before the snapshot is already in the base. A chunk cleaned after
    /// the snapshot is exported redundantly, which is safe.
    ///
    /// The copy order buys SELF-consistency only, not consistency with
    /// the coordinator: a racing flush that rebases + publishes AFTER
    /// our ref read leaves the export stamped one version BEHIND the
    /// ref coord holds, and the successor's lineage gate refuses a
    /// behind-stamped spool (2026-07-21 session-af28cac4 RCA — acked
    /// writes rolled back under a live guest). The SIGTERM path
    /// therefore aborts + reaps every in-flight final-flush task before
    /// the abandon sweep calls this (see
    /// `flush_nbd_data_planes_for_shutdown`); the copy order stays as
    /// defense-in-depth.
    pub async fn export_unflushed(&self) -> (ManifestRef, Vec<(usize, Vec<u8>)>) {
        let mut out = Vec::new();
        {
            let tier = self.dirty_tier.lock().await;
            let mut indices: Vec<usize> = tier.allocated_chunks().into_iter().collect();
            indices.sort_unstable();
            for chunk_idx in indices {
                match tier.read_chunk(
                    chunk_idx,
                    self.chunk_len(chunk_idx) as usize,
                    self.chunk_size,
                ) {
                    Ok(bytes) => out.push((chunk_idx, bytes.to_vec())),
                    Err(error) => {
                        tracing::error!(
                            path = %tier.path.display(),
                            chunk = chunk_idx,
                            %error,
                            "shutdown export could not read a dirty-file chunk; the file stays in place for successor recovery",
                        );
                    }
                }
            }
        }
        let manifest_ref = self.state.lock().await.manifest_ref;
        (manifest_ref, out)
    }

    /// Successor-side spool adoption: seed the dirty tier with the
    /// predecessor's exported chunks so its acked-but-un-uploaded
    /// writes survive the pod roll instead of being rolled back under
    /// the live guest. Pokes the threshold notify so an installed
    /// flush scheduler uploads promptly. Returns adopted bytes.
    ///
    /// ATOMIC: every chunk's shape is validated BEFORE anything lands in
    /// the dirty tier, and one out-of-shape chunk rejects the whole
    /// adoption (`AdoptShape`) with the tier untouched. The old behavior
    /// (warn + skip the bad chunk, adopt the rest) silently rolled back
    /// the skipped chunk's ACKED write — issue #810's trigger, surfaced
    /// only by the verify-on-read last line. A spool from a different
    /// lineage is still the CALLER's job to reject via the spool meta;
    /// this is the last-line shape check, now loud instead of lossy.
    pub async fn adopt_unflushed(
        &self,
        chunks: Vec<(usize, Vec<u8>)>,
    ) -> Result<u64, DiskBackendError> {
        for (idx, data) in &chunks {
            let start = (*idx as u64).saturating_mul(self.chunk_size);
            if start >= self.total_bytes || data.len() as u64 > self.chunk_size {
                return Err(DiskBackendError::AdoptShape {
                    chunk_idx: *idx,
                    len: data.len(),
                });
            }
        }
        let mut adopted = 0u64;
        {
            let mut tier = self.dirty_tier.lock().await;
            for (idx, data) in &chunks {
                adopted += data.len() as u64;
                tier.write_chunk(*idx, data, self.chunk_size)
                    .map_err(|source| dirty_file_error("adopt chunk", &tier.path, source))?;
            }
            for (idx, _) in chunks {
                tier.bump_generation(idx)?;
                tier.dirty.insert(idx);
            }
        }
        if adopted > 0 {
            self.threshold_notify.notify_one();
        }
        Ok(adopted)
    }

    /// Flush dirty chunks to the chunk store and tick the manifest
    /// version. The new `ManifestRef` is the durability gate the
    /// snapshot path attaches to `SnapshotRecord.disk_manifest`.
    ///
    /// After flush, unchanged claims are punched from the file and reads
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
        self.state.lock().await.fork_identity = Some(self.entropy.uuid());
    }

    /// ADR 0045 C1: see `migration_fence`.
    pub fn set_migration_fence(&self, fenced: bool) {
        self.migration_fence
            .store(fenced, std::sync::atomic::Ordering::SeqCst);
    }

    /// ADR 0038 B3 — phase 1 (runs under the FC pause on the snapshot
    /// path): capture and claim the current dirty set. Upload and base
    /// rebase remain post-resume.
    pub async fn flush_local(&self) -> Result<PendingDiskFlush, DiskBackendError> {
        // Issue #199: take the flush-pipeline guard before the dirty-tier
        // lock and hand it off inside the returned `PendingDiskFlush`, so the
        // drain and the `flush_upload` that publishes its chunks are one
        // serialized critical section. Without this, a slow upload from
        // an EARLIER drain could publish/rebase after a LATER drain
        // already did, overwriting the newer chunk with the older hash.
        let flush_guard = self.flush_pipeline.clone().lock_owned().await;
        let mut tier = self.dirty_tier.lock().await;
        if tier.dirty.is_empty() {
            // Nothing to publish ⟹ nothing to serialize; release the
            // pipeline guard immediately (don't carry it through an
            // empty no-op flush_upload, which would needlessly block a
            // concurrent flush).
            drop(tier);
            drop(flush_guard);
            return Ok(PendingDiskFlush {
                new_chunks: Vec::new(),
                claims: Vec::new(),
                flush_guard: None,
            });
        }
        let mut indices: Vec<usize> = tier.dirty.iter().copied().collect();
        indices.sort_unstable();
        let mut claims = Vec::with_capacity(indices.len());
        let mut new_chunks = Vec::with_capacity(indices.len());
        for &chunk_idx in &indices {
            let generation = tier.generations.get(&chunk_idx).copied().ok_or_else(|| {
                DiskBackendError::InvariantViolation(format!(
                    "dirty chunk {chunk_idx} has no generation"
                ))
            })?;
            let bytes = tier
                .read_chunk(
                    chunk_idx,
                    self.chunk_len(chunk_idx) as usize,
                    self.chunk_size,
                )
                .map_err(|source| dirty_file_error("read dirty chunk", &tier.path, source))?;
            let hash = ChunkHash::of(&bytes);
            new_chunks.push((chunk_idx, hash, bytes));
            claims.push(DirtyChunkClaim {
                chunk_idx,
                generation,
            });
        }
        for claim in &claims {
            tier.dirty.remove(&claim.chunk_idx);
            tier.claimed.insert(claim.chunk_idx, claim.generation);
        }
        // The seam fires while the tier lock still protects the claim.
        self.fire_flush_seam(FlushSeamPoint::DirtyPendingHandoff)
            .await;
        drop(tier);
        Ok(PendingDiskFlush {
            new_chunks,
            claims,
            flush_guard: Some(flush_guard),
        })
    }

    /// Arm the flush scheduler seam at `point` (ADR 0098 P6). The
    /// returned `(arrived, proceed)` pair lets the caller park the
    /// pipeline at that instant and interleave against it: `arrived`
    /// fires when the pipeline reaches the point; the pipeline blocks
    /// until `proceed` is notified. One-shot: firing disarms it.
    pub fn arm_flush_seam(&self, point: FlushSeamPoint) -> (Arc<Notify>, Arc<Notify>) {
        let arrived = Arc::new(Notify::new());
        let proceed = Arc::new(Notify::new());
        *self.scheduler_seam.lock().unwrap() = Some(ArmedFlushSeam {
            point,
            arrived: arrived.clone(),
            proceed: proceed.clone(),
        });
        (arrived, proceed)
    }

    /// Fire the seam if one is armed at `point` — park until `proceed`.
    /// A seam armed at a DIFFERENT point stays armed untouched.
    async fn fire_flush_seam(&self, point: FlushSeamPoint) {
        let seam = {
            let mut armed = self.scheduler_seam.lock().unwrap();
            match armed.as_ref() {
                Some(a) if a.point == point => armed.take(),
                _ => None,
            }
        };
        if let Some(seam) = seam {
            seam.arrived.notify_one();
            let proceed = seam.proceed.notified();
            tokio::pin!(proceed);
            proceed.await;
        }
    }

    /// ADR 0038 B3 — phase 2 (runs post-resume on the snapshot path):
    /// upload the chunks captured by `flush_local` to GCS, then publish
    /// the manifest and rebase `base`. Keeping the rebase *after* the
    /// upload preserves "a manifest someone restores from ⟹ its chunks
    /// are durable" — the background scheduler reads `base`, so it never
    /// references a not-yet-uploaded chunk. A failed upload releases the
    /// claims to the dirty set so the next flush retries them.
    /// ADR 0045 C1: the migration flavor of `flush_upload` — land the
    /// claimed chunks in the host-local NVMe cache ONLY (no GCS PUT on
    /// the teleport pause path) and return the post-drain manifest
    /// WITHOUT publishing or rebasing. The manifest's chunks are
    /// reachable through the cache for the destination's pull; the
    /// destination's durability catch-up uploads + publishes later.
    /// The source's own `state` is untouched: on commit the VM is
    /// destroyed; on abort call [`Self::requeue_pending`] first.
    ///
    /// Returns `(manifest, new_hashes)` for the destination pull.
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

    /// ADR 0045 C1 abort path: release the claimed chunks to the dirty
    /// set so the resumed guest's next flush
    /// retries them (a newer guest write wins — same rule as the
    /// upload-failure re-queue). Idempotent.
    pub async fn requeue_pending(&self, pending: PendingDiskFlush) {
        let mut tier = self.dirty_tier.lock().await;
        tier.release_claims(&pending.claims);
    }

    /// ADR 0045 C2 disk post-copy — the SOURCE seal (runs under the
    /// FC pause; the blackout's disk leg). Snapshots every chunk whose
    /// guest-visible content is NOT reproducible from the published
    /// base manifest: all dirty and claimed file chunks. The guest is
    /// paused and `wait_idle` has completed. An in-flight upload can
    /// still clean its claim because the seal owns its own bytes. NO hashing,
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
        let mut chunks = HashMap::new();
        {
            let mut tier = self.dirty_tier.lock().await;
            let mut indices: Vec<usize> = tier.allocated_chunks().into_iter().collect();
            indices.sort_unstable();
            for chunk_idx in indices {
                match tier.read_chunk(
                    chunk_idx,
                    self.chunk_len(chunk_idx) as usize,
                    self.chunk_size,
                ) {
                    Ok(bytes) => {
                        chunks.insert(chunk_idx, bytes);
                    }
                    Err(error) => {
                        tracing::error!(
                            path = %tier.path.display(),
                            chunk = chunk_idx,
                            %error,
                            "post-copy seal could not read a dirty-file chunk",
                        );
                    }
                }
            }
            tier.dirty.clear();
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
        let mut tier = self.dirty_tier.lock().await;
        for (idx, bytes) in &seal.chunks {
            if tier.contains(*idx) {
                continue;
            }
            if let Err(error) = tier.write_chunk(*idx, bytes, self.chunk_size) {
                tracing::error!(
                    path = %tier.path.display(),
                    chunk = *idx,
                    %error,
                    "post-copy abort could not restore a dirty-file chunk",
                );
                continue;
            }
            if let Err(error) = tier.bump_generation(*idx) {
                tracing::error!(chunk = *idx, %error, "post-copy abort generation failed");
                continue;
            }
            tier.dirty.insert(*idx);
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
            let mut tier = self.dirty_tier.lock().await;
            if tier.contains(chunk_idx) {
                tier.read_chunk(
                    chunk_idx,
                    self.chunk_len(chunk_idx) as usize,
                    self.chunk_size,
                )
                .map_err(|source| dirty_file_error("read post-copy chunk", &tier.path, source))?
            } else {
                tier.write_chunk(chunk_idx, &bytes, self.chunk_size)
                    .map_err(|source| {
                        dirty_file_error("write post-copy chunk", &tier.path, source)
                    })?;
                tier.bump_generation(chunk_idx)?;
                tier.dirty.insert(chunk_idx);
                bytes
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
            let started = crate::time_source::metrics_now();
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
        let claims = pending.claims;
        let new_chunks = pending.new_chunks;
        if claims.is_empty() {
            // Incident 2026-07-10 follow-up: an ARMED fork (ADR 0077 —
            // fresh restore off a SHARED base manifest) must still
            // materialize on an empty flush. The eviction capture records
            // this outcome's ref as the session's disk lineage; returning
            // the shared base ref for a session that never dirtied a
            // chunk would resume it UNFORKED, and its post-resume flushes
            // would tick the shared chain again. Publish the private id
            // at v1 with the base's exact chunk list — one manifest PUT,
            // no chunk uploads. (Residual gap, deliberate: a pod roll
            // before ANY flush loses the in-memory `fork_identity`; the
            // rehydrated backend can't distinguish shared-vs-private refs
            // and re-attaches unforked. The window is one flush interval.)
            let mut state = self.state.lock().await;
            if let Some(fork_id) = state.fork_identity {
                let forked_ref = ManifestRef {
                    manifest_id: fork_id,
                    version: 1,
                };
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
                    parent: Some(state.manifest_ref),
                    working_set_trace: None,
                    annotations: serde_json::Value::Null,
                };
                self.store.put_manifest(forked_ref, &manifest).await?;
                state.manifest_ref = forked_ref;
                state.fork_identity = None;
                tracing::info!(
                    manifest = %forked_ref,
                    "empty flush materialized the armed manifest fork (no dirty chunks)",
                );
            }
            let out = DiskFlushOutcome {
                manifest_ref: state.manifest_ref,
                chunks_flushed: 0,
                bytes_uploaded: 0,
            };
            drop(state);
            self.stamp_flush_completion();
            return Ok(out);
        }
        // Upload the pause-time file snapshot outside the frozen-guest path.
        // ADR 0039 item #19: the per-chunk put dominates the flush
        // (~16 ms each), so awaiting one at a time made a large dirty
        // set (~1,936 chunks → ~32 s) the slow phase of an eviction.
        // Puts are content-addressed/idempotent and keyed by chunk_idx,
        // so they're order-independent — fan them out with bounded
        // `buffer_unordered`.
        //
        // Each task uploads the file bytes captured above. The manifest
        // uses hashes from that same snapshot.
        //
        // ATOMICITY (durability invariant): `try_collect` aborts on the
        // first put error and the manifest rebuild runs only after every
        // put succeeds — a published manifest never references a
        // not-yet-durable chunk. On failure we release the claims so the next checkpoint
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
                        // ADR 0078 move 5: dirty flush chunks are freshly
                        // re-chunked and new by construction — skip the
                        // per-chunk `exists()` GCS HEAD that always missed.
                        let put = store.put_chunk_unchecked(&bytes);
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
                        // ADR 0039 (sticky-everywhere): write-through to
                        // the local cache so the flushing host keeps its
                        // OWN just-uploaded chunks. Rides the backend's
                        // explicit cache handle: the store's internal
                        // write-through (ADR 0078 move 5) only fires when
                        // the store was built with a cache wired — prod
                        // wiring, but not a structural guarantee (cache
                        // and store travel separately into this backend).
                        // Prod double-writes 16 MiB per chunk as a result;
                        // collapsing to one cache identity is substrated
                        // (ADR 0076) territory, not this PR's. Best-effort:
                        // the chunk is durable in GCS, so a cache-write
                        // failure is a missed optimization, never
                        // incorrect (reads fall back).
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
                let mut tier = self.dirty_tier.lock().await;
                tier.release_claims(&claims);
                tracing::warn!(
                    chunks = new_chunks.len(),
                    error = %e,
                    "nbd disk flush_upload failed; manifest NOT advanced, dirty re-queued (no partial snapshot)",
                );
                return Err(e);
            }
        }

        // Issue #199 seam point: every put is durable; the fence re-check
        // and the publish are still ahead — a fence raised while parked
        // here must abort the publish exactly like one raised mid-upload.
        self.fire_flush_seam(FlushSeamPoint::PostUploadPrePublish)
            .await;

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
        // The claims return to the dirty set, so reads stay file-backed.
        if self
            .migration_fence
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            let manifest_ref = state.manifest_ref;
            drop(state);
            let mut tier = self.dirty_tier.lock().await;
            tier.release_claims(&claims);
            drop(tier);
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
                        let error = engram_chunk_store::ChunkStoreError::VersionConflict {
                            latest,
                            attempted,
                            manifest_id,
                        };
                        drop(state);
                        let mut tier = self.dirty_tier.lock().await;
                        tier.release_claims(&claims);
                        return Err(error.into());
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
                Err(error) => {
                    drop(state);
                    let mut tier = self.dirty_tier.lock().await;
                    tier.release_claims(&claims);
                    return Err(error.into());
                }
            }
        };

        // Store-ahead seam point: the manifest is published but nothing
        // local (or coordinator-side) has learned it yet — a crash parked
        // here is the 85e0298a store-ahead shape; the version-conflict
        // retry above is the recovery. The `state` lock is deliberately
        // still held (a parked crash drops it with the task).
        self.fire_flush_seam(FlushSeamPoint::PreRebase).await;

        // A publish result is not enough to delete local data. Read the
        // manifest back and verify every uploaded chunk before cleanup.
        let published = match self.store.get_manifest(new_ref).await {
            Ok(manifest) => manifest,
            Err(error) => {
                drop(state);
                let mut tier = self.dirty_tier.lock().await;
                tier.release_claims(&claims);
                return Err(error.into());
            }
        };
        let verified = new_chunks.iter().all(|(idx, hash, _)| {
            let offset = *idx as u64 * chunk_size;
            published
                .chunks
                .iter()
                .any(|chunk| chunk.offset == offset && chunk.hash == *hash)
        });
        if !verified {
            drop(state);
            let mut tier = self.dirty_tier.lock().await;
            tier.release_claims(&claims);
            return Err(DiskBackendError::InvariantViolation(format!(
                "published manifest {new_ref} did not contain every uploaded dirty chunk"
            )));
        }

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

        {
            let mut tier = self.dirty_tier.lock().await;
            for claim in &claims {
                if tier.claimed.get(&claim.chunk_idx) != Some(&claim.generation) {
                    continue;
                }
                tier.claimed.remove(&claim.chunk_idx);
                let unchanged = tier.generations.get(&claim.chunk_idx) == Some(&claim.generation)
                    && !tier.dirty.contains(&claim.chunk_idx);
                if unchanged {
                    let offset = claim.chunk_idx as u64 * chunk_size;
                    if let Err(source) =
                        punch_hole(&tier.file, offset, self.chunk_len(claim.chunk_idx))
                    {
                        tier.dirty.insert(claim.chunk_idx);
                        tracing::warn!(
                            path = %tier.path.display(),
                            chunk = claim.chunk_idx,
                            error = %source,
                            "could not punch a published dirty chunk; chunk remains dirty",
                        );
                    }
                } else {
                    tier.dirty.insert(claim.chunk_idx);
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
        // Dirty and claimed chunks stay in the same file. The tier lock
        // keeps a read atomic with a write or a claim cleanup.
        {
            let tier = self.dirty_tier.lock().await;
            if tier.contains(chunk_idx) {
                return tier
                    .read_chunk(chunk_idx, chunk_len as usize, self.chunk_size)
                    .map_err(|source| dirty_file_error("read guest chunk", &tier.path, source));
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
                        if bytes.len() as u64 != chunk_len {
                            return Err(DiskBackendError::ShortChunk {
                                chunk_idx,
                                expected: chunk_len,
                                actual: bytes.len(),
                            });
                        }
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
                if bytes.len() as u64 != chunk_len {
                    return Err(DiskBackendError::ShortChunk {
                        chunk_idx,
                        expected: chunk_len,
                        actual: bytes.len(),
                    });
                }
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
    // tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]
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

    fn test_cache(path: PathBuf) -> ChunkCache {
        let mut cfg = ChunkCacheConfig::new(path);
        cfg.budget_bytes = 64 * 1024 * 1024;
        ChunkCache::new(cfg)
    }

    /// Keep the dirty file when a test drops its backend.
    async fn retain_dirty_path(backend: &ChunkedDiskBackend) -> PathBuf {
        let mut tier = backend.dirty_tier.lock().await;
        // Sync so the successor's extent scan does not depend on
        // writeback timing. Recovery needs exact extents, which ext4
        // gives and APFS does not (it reports flushed zero-fill as
        // data) — so the Recover-mode tests are Linux-gated.
        tier.file.sync_all().unwrap();
        tier.remove_on_drop = false;
        tier.path.clone()
    }

    /// Synth helper: put a chunk of `byte` repeated `size` times
    /// into the store and return its hash. Lets tests build a base
    /// manifest with known content.
    async fn put_chunk(store: &ChunkStore, byte: u8, size: usize) -> ChunkHash {
        let bytes = vec![byte; size];
        store.put_chunk(&bytes).await.unwrap()
    }

    struct ProcessRecoveryContext {
        manifest: Manifest,
        cache_root: std::path::PathBuf,
        store: Arc<ChunkStore>,
    }

    #[derive(Clone, Copy, Debug)]
    enum SpoolCrashState {
        Pristine,
        TornChunk,
        MissingMarker,
        MissingChunk,
        ForeignGarbage,
    }

    enum ProcessDeathState {
        /// Dirty-file recovery needs exact extents (ext4), so these
        /// two states are exercised on Linux only.
        #[cfg(target_os = "linux")]
        Intact,
        #[cfg(target_os = "linux")]
        WriteAfterFlushStarted {
            offset: u64,
            bytes: Vec<u8>,
        },
        Malformed,
        Spool(SpoolCrashState),
    }

    struct ProcessRecoveryOutcome {
        backend: ChunkedDiskBackend,
        recovery: Result<(), String>,
    }

    /// Simulate process death and recover from the dirty file or the
    /// pre-upgrade spool fallback.
    async fn survive_process_death(
        backend: ChunkedDiskBackend,
        context: &ProcessRecoveryContext,
        state: ProcessDeathState,
    ) -> ProcessRecoveryOutcome {
        use crate::disk_daemon::spool;

        #[cfg(target_os = "linux")]
        let mut interrupted_flush = None;
        #[cfg(target_os = "linux")]
        let state = match state {
            ProcessDeathState::WriteAfterFlushStarted { offset, bytes } => {
                interrupted_flush = Some(backend.flush_local().await.unwrap());
                backend.write(offset, &bytes).await.unwrap();
                ProcessDeathState::Intact
            }
            state => state,
        };
        match state {
            #[cfg(target_os = "linux")]
            ProcessDeathState::Intact => {
                let successor_ref = backend.manifest_ref().await;
                let dirty_path = retain_dirty_path(&backend).await;
                drop(interrupted_flush);
                drop(backend);

                let successor = ChunkedDiskBackend::from_manifest_with_dirty_file(
                    successor_ref,
                    &context.manifest,
                    test_cache(
                        context
                            .cache_root
                            .join(format!("process-recovery-{}", rand_suffix())),
                    ),
                    context.store.clone(),
                    u64::MAX,
                    dirty_path,
                    DirtyFileOpenMode::Recover,
                )
                .unwrap();
                ProcessRecoveryOutcome {
                    backend: successor,
                    recovery: Ok(()),
                }
            }
            ProcessDeathState::Malformed | ProcessDeathState::Spool(_) => {
                let (exported_ref, exported_chunks) = backend.export_unflushed().await;
                let dirty_path = backend.dirty_tier.lock().await.path.clone();
                let (spool_chunks, crash_state) = match state {
                    ProcessDeathState::Malformed => (
                        vec![
                            (0, vec![0x33; 16]),
                            (7, vec![0x11; 4096]),
                            (
                                1,
                                vec![0x22; context.manifest.chunk_size.as_u64() as usize * 2],
                            ),
                        ],
                        None,
                    ),
                    ProcessDeathState::Spool(crash_state) => (exported_chunks, Some(crash_state)),
                    #[cfg(target_os = "linux")]
                    ProcessDeathState::Intact
                    | ProcessDeathState::WriteAfterFlushStarted { .. } => unreachable!(),
                };
                let sid = engram_core::SandboxId::new();
                let spool_root = tempfile::tempdir().unwrap();
                let root = spool_root.path();
                let sandbox_dir = root.join(sid.to_string());
                let written = spool::write_spool(
                    &engram_host_core::TokioFs,
                    root,
                    sid,
                    exported_ref,
                    &spool_chunks,
                )
                .await
                .map_err(|error| error.to_string());

                if written.is_ok() {
                    if let Some(crash_state) = crash_state {
                        match crash_state {
                            SpoolCrashState::Pristine => {}
                            SpoolCrashState::TornChunk => {
                                std::fs::write(sandbox_dir.join("chunk-0.bin"), [0x11; 100])
                                    .unwrap();
                            }
                            SpoolCrashState::MissingMarker => {
                                std::fs::remove_file(sandbox_dir.join("meta.json")).unwrap();
                            }
                            SpoolCrashState::MissingChunk => {
                                std::fs::remove_file(sandbox_dir.join("chunk-2.bin")).unwrap();
                            }
                            SpoolCrashState::ForeignGarbage => {
                                std::fs::write(
                                    sandbox_dir.join("chunk-tmp.swp"),
                                    b"editor droppings",
                                )
                                .unwrap();
                                std::fs::write(
                                    sandbox_dir.join("chunk-0.bin.partial"),
                                    b"torn tmp",
                                )
                                .unwrap();
                            }
                        }
                    }
                }

                let recovery_data = match written {
                    Err(error) => Err(error),
                    Ok(_) => match spool::read_spool(&engram_host_core::TokioFs, root, sid).await {
                        Ok(Some((meta, chunks))) => Ok((
                            ManifestRef {
                                manifest_id: meta.manifest_id,
                                version: meta.version,
                            },
                            chunks,
                        )),
                        Ok(None) => Err("recovery state is incomplete".into()),
                        Err(error) => Err(error.to_string()),
                    },
                };

                std::fs::remove_file(dirty_path).unwrap();
                #[cfg(target_os = "linux")]
                drop(interrupted_flush);
                drop(backend);

                let successor_ref = recovery_data
                    .as_ref()
                    .map(|(manifest_ref, _)| *manifest_ref)
                    .unwrap_or(exported_ref);
                let successor = ChunkedDiskBackend::new(
                    successor_ref,
                    &context.manifest,
                    test_cache(
                        context
                            .cache_root
                            .join(format!("process-recovery-{}", rand_suffix())),
                    ),
                    context.store.clone(),
                    u64::MAX,
                )
                .unwrap();
                let recovery = match recovery_data {
                    Ok((_manifest_ref, chunks)) => successor
                        .adopt_unflushed(chunks)
                        .await
                        .map(|_| ())
                        .map_err(|error| error.to_string()),
                    Err(error) => Err(error),
                };
                ProcessRecoveryOutcome {
                    backend: successor,
                    recovery,
                }
            }
            #[cfg(target_os = "linux")]
            ProcessDeathState::WriteAfterFlushStarted { .. } => unreachable!(),
        }
    }

    #[tokio::test]
    async fn read_returns_short_chunk_error_for_manifest_width_mismatch() {
        let chunk_size = 4096u64;
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let hash = store.put_chunk(&[0xaa; 100]).await.unwrap();
        let manifest = synth_manifest(chunk_size, chunk_size, vec![(0, hash)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap();

        assert!(matches!(
            backend.read(0, chunk_size).await,
            Err(DiskBackendError::ShortChunk {
                chunk_idx: 0,
                expected: 4096,
                actual: 100,
            })
        ));
    }

    /// 2026-07-16 session-85e0298a RCA: every acked write must remain
    /// readable after process death, including a write that lands while
    /// an earlier flush is in progress.
    /// Needs exact SEEK_DATA extents (ext4). APFS reports flushed
    /// zero-fill as data, so this test runs on Linux only (the CI
    /// Linux lane and `just test-linux engram-host-agent`).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn acked_writes_survive_process_death_across_backends() {
        let chunk_size = 64 * 1024u64;
        let total = 3 * chunk_size;
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
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
        let backend = ChunkedDiskBackend::new(
            manifest_ref,
            &manifest,
            ChunkCache::new(cfg),
            store.clone(),
            u64::MAX,
        )
        .unwrap();
        let context = ProcessRecoveryContext {
            manifest,
            cache_root: dir.path().join("successor-cache"),
            store,
        };

        backend
            .write(0, &vec![0x11; chunk_size as usize])
            .await
            .unwrap();
        backend
            .write(2 * chunk_size, &vec![0x33; chunk_size as usize])
            .await
            .unwrap();
        let outcome = survive_process_death(
            backend,
            &context,
            ProcessDeathState::WriteAfterFlushStarted {
                offset: 0,
                bytes: vec![0x22; chunk_size as usize],
            },
        )
        .await;
        outcome.recovery.unwrap();

        let mut expected = vec![0xaa; total as usize];
        expected[chunk_size as usize..(2 * chunk_size) as usize].fill(0xbb);
        expected[(2 * chunk_size) as usize..].fill(0x33);
        expected[..chunk_size as usize].fill(0x22);
        assert_eq!(outcome.backend.read(0, total).await.unwrap(), expected);
    }

    #[tokio::test]
    async fn malformed_recovery_fails_loudly_and_atomically() {
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
        let backend = ChunkedDiskBackend::new(
            manifest_ref,
            &manifest,
            ChunkCache::new(cfg),
            store.clone(),
            u64::MAX,
        )
        .unwrap();
        let context = ProcessRecoveryContext {
            manifest,
            cache_root: dir.path().join("successor-cache"),
            store,
        };

        let outcome = survive_process_death(backend, &context, ProcessDeathState::Malformed).await;
        assert!(
            outcome.recovery.is_err(),
            "malformed recovery data must fail loudly"
        );
        assert_eq!(
            outcome.backend.read(0, chunk_size).await.unwrap(),
            vec![0xaa; chunk_size as usize],
            "failed recovery must leave the base image unchanged"
        );
    }

    /// ADR 0099 H5: each recoverable process-crash state reproduces the
    /// complete acked disk. Every other state fails loudly.
    #[tokio::test]
    async fn acked_writes_never_silently_regress_across_process_crash_states() {
        let chunk_size = 4096u64;
        let total = 3 * chunk_size;
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
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
        let context = ProcessRecoveryContext {
            manifest: manifest.clone(),
            cache_root: dir.path().join("successor-cache"),
            store: store.clone(),
        };
        let mut expected = vec![0xaa; total as usize];
        expected[chunk_size as usize..(2 * chunk_size) as usize].fill(0xbb);
        expected[(2 * chunk_size) as usize..].fill(0x33);
        expected[..chunk_size as usize].fill(0x11);

        for (state, must_recover) in [
            (SpoolCrashState::Pristine, true),
            (SpoolCrashState::TornChunk, false),
            (SpoolCrashState::MissingMarker, false),
            (SpoolCrashState::MissingChunk, false),
            (SpoolCrashState::ForeignGarbage, true),
        ] {
            let mut source_cfg =
                ChunkCacheConfig::new(dir.path().join(format!("source-cache-{}", rand_suffix())));
            source_cfg.budget_bytes = 64 * 1024 * 1024;
            let source = ChunkedDiskBackend::new(
                manifest_ref,
                &manifest,
                ChunkCache::new(source_cfg),
                store.clone(),
                u64::MAX,
            )
            .unwrap();
            source.write(0, &[0x11; 4096]).await.unwrap();
            source.write(2 * chunk_size, &[0x33; 4096]).await.unwrap();

            let outcome =
                survive_process_death(source, &context, ProcessDeathState::Spool(state)).await;
            match outcome.recovery {
                Ok(()) => assert_eq!(
                    outcome.backend.read(0, total).await.unwrap(),
                    expected,
                    "recovery from {state:?} served stale or incomplete bytes"
                ),
                Err(error) => assert!(
                    !must_recover,
                    "recovery from {state:?} failed unexpectedly: {error}"
                ),
            }
        }
    }

    /// Recovery keeps writes in chunks that an interrupted flush claimed.
    /// Needs exact SEEK_DATA extents (ext4). APFS reports flushed
    /// zero-fill as data, so this test runs on Linux only (the CI
    /// Linux lane and `just test-linux engram-host-agent`).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn acked_writes_survive_death_with_claims_in_flight() {
        let chunk_size = 64 * 1024u64;
        let total = 3 * chunk_size;
        let manifest = synth_manifest(total, chunk_size, vec![]);
        let (backend, store, dir) = build_backend(&manifest).await;
        let manifest_ref = backend.manifest_ref().await;

        backend
            .write(0, &vec![0x11; chunk_size as usize])
            .await
            .unwrap();
        backend
            .write(2 * chunk_size, &vec![0x33; chunk_size as usize])
            .await
            .unwrap();
        let pending = backend.flush_local().await.unwrap();
        backend
            .write(chunk_size / 2, &vec![0x22; chunk_size as usize / 2])
            .await
            .unwrap();

        let dirty_path = retain_dirty_path(&backend).await;
        drop(pending);
        drop(backend);

        let successor = ChunkedDiskBackend::from_manifest_with_dirty_file(
            manifest_ref,
            &manifest,
            test_cache(dir.path().join("claims-recovery-cache")),
            store,
            u64::MAX,
            dirty_path,
            DirtyFileOpenMode::Recover,
        )
        .unwrap();
        let mut expected = vec![0; total as usize];
        expected[..chunk_size as usize / 2].fill(0x11);
        expected[chunk_size as usize / 2..chunk_size as usize].fill(0x22);
        expected[(2 * chunk_size) as usize..].fill(0x33);

        assert_eq!(successor.read(0, total).await.unwrap(), expected);
    }

    /// A verified publish removes dirty extents and keeps reads correct.
    /// Needs exact SEEK_DATA extents (ext4). APFS reports flushed
    /// zero-fill as data, so this test runs on Linux only (the CI
    /// Linux lane and `just test-linux engram-host-agent`).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn verified_publish_punches_holes_and_reads_stay_correct() {
        let chunk_size = 64 * 1024u64;
        let total = 2 * chunk_size;
        let manifest = synth_manifest(total, chunk_size, vec![]);
        let (backend, store, dir) = build_backend(&manifest).await;
        let dirty_path = retain_dirty_path(&backend).await;
        let mut expected = vec![0; total as usize];
        expected[chunk_size as usize..].fill(0x5a);

        backend
            .write(chunk_size, &vec![0x5a; chunk_size as usize])
            .await
            .unwrap();
        let outcome = backend.flush().await.unwrap();
        assert_eq!(backend.read(0, total).await.unwrap(), expected);

        let published = store.get_manifest(outcome.manifest_ref).await.unwrap();
        drop(backend);
        let successor = ChunkedDiskBackend::from_manifest_with_dirty_file(
            outcome.manifest_ref,
            &published,
            test_cache(dir.path().join("punched-recovery-cache")),
            store,
            u64::MAX,
            dirty_path,
            DirtyFileOpenMode::Recover,
        )
        .unwrap();

        assert_eq!(successor.dirty_chunks_count().await, 0);
        assert_eq!(successor.read(0, total).await.unwrap(), expected);
    }

    /// A rewrite during upload stays dirty and is published by the next flush.
    /// Needs exact SEEK_DATA extents (ext4). APFS reports flushed
    /// zero-fill as data, so this test runs on Linux only (the CI
    /// Linux lane and `just test-linux engram-host-agent`).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn chunk_rewritten_during_upload_is_not_punched_and_reuploads() {
        let chunk_size = 4096u64;
        let manifest = synth_manifest(chunk_size, chunk_size, vec![]);
        let (backend, store, dir) = build_backend(&manifest).await;
        let backend = Arc::new(backend);
        let dirty_path = retain_dirty_path(&backend).await;

        backend.write(0, &[0x11; 4096]).await.unwrap();
        let (arrived, proceed) = backend.arm_flush_seam(FlushSeamPoint::PostUploadPrePublish);
        let flush_task = {
            let backend = backend.clone();
            tokio::spawn(async move { backend.flush().await })
        };
        arrived.notified().await;
        backend.write(chunk_size / 2, &[0x22; 2048]).await.unwrap();
        proceed.notify_one();
        flush_task.await.unwrap().unwrap();

        let mut expected = vec![0x11; chunk_size as usize];
        expected[chunk_size as usize / 2..].fill(0x22);
        assert_eq!(backend.read(0, chunk_size).await.unwrap(), expected);
        assert_eq!(backend.dirty_chunks_count().await, 1);

        let outcome = backend.flush().await.unwrap();
        assert_eq!(outcome.chunks_flushed, 1);
        assert_eq!(backend.read(0, chunk_size).await.unwrap(), expected);
        let published = store.get_manifest(outcome.manifest_ref).await.unwrap();
        drop(backend);

        let successor = ChunkedDiskBackend::from_manifest_with_dirty_file(
            outcome.manifest_ref,
            &published,
            test_cache(dir.path().join("rewrite-recovery-cache")),
            store,
            u64::MAX,
            dirty_path,
            DirtyFileOpenMode::Recover,
        )
        .unwrap();
        assert_eq!(successor.dirty_chunks_count().await, 0);
        assert_eq!(successor.read(0, chunk_size).await.unwrap(), expected);
    }

    /// A publish that omits uploaded data cannot remove the dirty extent.
    #[tokio::test]
    async fn publish_that_omits_a_chunk_never_punches() {
        let chunk_size = 4096u64;
        let manifest = synth_manifest(chunk_size, chunk_size, vec![]);
        let dir = tempfile::tempdir().unwrap();
        let omit_next_manifest_read = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let blob: Arc<dyn BlobStorage> = Arc::new(OmitChunkOnManifestReadBlob {
            inner: LocalBlobStorage::new(dir.path().join("blob")),
            omit_next_manifest_read: omit_next_manifest_read.clone(),
        });
        let store = Arc::new(ChunkStore::new(blob));
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let backend = ChunkedDiskBackend::new(
            manifest_ref,
            &manifest,
            test_cache(dir.path().join("cache")),
            store.clone(),
            u64::MAX,
        )
        .unwrap();
        let expected = vec![0x6b; chunk_size as usize];
        backend.write(0, &expected).await.unwrap();

        omit_next_manifest_read.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            backend.flush().await,
            Err(DiskBackendError::InvariantViolation(_))
        ));
        assert_eq!(backend.read(0, chunk_size).await.unwrap(), expected);

        let dirty_path = retain_dirty_path(&backend).await;
        let successor_ref = backend.manifest_ref().await;
        drop(backend);
        let successor = ChunkedDiskBackend::from_manifest_with_dirty_file(
            successor_ref,
            &manifest,
            test_cache(dir.path().join("omitted-recovery-cache")),
            store,
            u64::MAX,
            dirty_path,
            DirtyFileOpenMode::Recover,
        )
        .unwrap();

        assert_eq!(successor.dirty_chunks_count().await, 1);
        assert_eq!(successor.read(0, chunk_size).await.unwrap(), expected);
    }

    /// Recovery marks the whole chunk for a partial allocated extent.
    /// Needs exact SEEK_DATA extents (ext4). APFS reports flushed
    /// zero-fill as data, so this test runs on Linux only (the CI
    /// Linux lane and `just test-linux engram-host-agent`).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn extent_scan_marks_partial_extents_as_whole_chunks() {
        let chunk_size = 64 * 1024u64;
        let total = 4 * chunk_size;
        let manifest = synth_manifest(total, chunk_size, vec![]);
        let dir = tempfile::tempdir().unwrap();
        let dirty_path = dir.path().join("out-of-band-dirty.cache");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&dirty_path)
            .unwrap();
        file.set_len(total).unwrap();
        let middle = 173usize;
        let bytes = [0xa1, 0xb2, 0xc3, 0xd4, 0xe5];
        write_all_at(
            &file,
            &bytes,
            2 * chunk_size + u64::try_from(middle).unwrap(),
        )
        .unwrap();
        // Same APFS rule as retain_dirty_path: flush before the scan.
        file.sync_all().unwrap();
        drop(file);

        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().join("blob")));
        let store = Arc::new(ChunkStore::new(blob));
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let backend = ChunkedDiskBackend::from_manifest_with_dirty_file(
            manifest_ref,
            &manifest,
            test_cache(dir.path().join("cache")),
            store,
            u64::MAX,
            dirty_path,
            DirtyFileOpenMode::Recover,
        )
        .unwrap();

        assert_eq!(backend.dirty_chunks_count().await, 1);
        let mut expected = vec![0; chunk_size as usize];
        expected[middle..middle + bytes.len()].copy_from_slice(&bytes);
        assert_eq!(
            backend.read(2 * chunk_size, chunk_size).await.unwrap(),
            expected
        );
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

    /// Incident 2026-07-10 follow-up: an ARMED fork materializes even on
    /// an EMPTY flush. A base-restored session that never dirtied a chunk
    /// gets evicted → the capture's flush records the outcome ref as its
    /// disk lineage — that ref must be the PRIVATE id (published, v1),
    /// never the shared base ref, or the resume re-attaches unforked and
    /// post-resume flushes tick the shared chain again.
    #[tokio::test]
    async fn armed_fork_materializes_on_empty_flush() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let manifest = synth_manifest(chunk_size, chunk_size, vec![(0, h0)]);
        let base_ref = ManifestRef::new();
        store.put_manifest(base_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache-empty-fork"));
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

        // NO writes. The flush is empty — but the fork must still mint.
        let out = backend.flush().await.unwrap();
        assert_eq!(out.chunks_flushed, 0);
        assert_ne!(
            out.manifest_ref.manifest_id, base_ref.manifest_id,
            "empty flush must materialize the armed fork, not return the shared base ref",
        );
        assert_eq!(out.manifest_ref.version, 1);
        // The private manifest is PUBLISHED (resolvable) and carries the
        // base's exact chunk list.
        let forked = store.get_manifest(out.manifest_ref).await.unwrap();
        assert_eq!(forked.chunks.len(), 1);
        assert_eq!(forked.chunks[0].hash, h0);
        // The shared base chain was never ticked.
        assert!(store.get_manifest(base_ref.next_version()).await.is_err());
        // A later real write ticks the private id (no re-fork).
        backend.write(0, &[0x44u8; 8]).await.unwrap();
        let out2 = backend.flush().await.unwrap();
        assert_eq!(out2.manifest_ref.manifest_id, out.manifest_ref.manifest_id);
        assert_eq!(out2.manifest_ref.version, 2);
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

        // Zero-dirty flush (the evict-with-no-writes shape): the armed
        // fork MATERIALIZES (incident 2026-07-10 — returning the shared
        // base ref here resumed the session unforked), and the property
        // this test exists for still holds: the outcome ref is PUBLISHED,
        // never a dangling placeholder.
        let out = backend.flush().await.unwrap();
        assert_eq!(out.chunks_flushed, 0);
        assert_ne!(
            out.manifest_ref.manifest_id, base_ref.manifest_id,
            "zero-dirty flush must materialize the armed fork, not echo the shared base",
        );
        assert_eq!(out.manifest_ref.version, 1);
        store
            .get_manifest(out.manifest_ref)
            .await
            .expect("zero-dirty flush outcome must resolve in the store");

        // The first real write TICKS the (already-materialized) private
        // identity — and that resolves too.
        backend.write(0, &[0x55u8; 4096]).await.unwrap();
        let out2 = backend.flush().await.unwrap();
        assert_eq!(out2.manifest_ref.manifest_id, out.manifest_ref.manifest_id);
        assert_eq!(out2.manifest_ref.version, 2);
        store
            .get_manifest(out2.manifest_ref)
            .await
            .expect("first real publish must resolve in the store");
        assert_eq!(backend.manifest_ref().await, out2.manifest_ref);
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

    /// A write that races a flush must preserve all earlier acked bytes.
    #[tokio::test]
    async fn write_racing_flush_reads_the_newest_acked_bytes() {
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
        let backend = Arc::new(
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store.clone(), u64::MAX)
                .unwrap(),
        );

        backend.write(0, &[0x11u8; 2048]).await.unwrap();
        let (arrived, proceed) = backend.arm_flush_seam(FlushSeamPoint::DirtyPendingHandoff);
        let flush_backend = backend.clone();
        let flush_task = tokio::spawn(async move { flush_backend.flush().await });
        arrived.notified().await;

        let write_backend = backend.clone();
        let write_task =
            tokio::spawn(async move { write_backend.write(2048, &[0x22u8; 2048]).await });
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        proceed.notify_one();

        flush_task.await.unwrap().unwrap();
        write_task.await.unwrap().unwrap();

        let bytes = backend.read(0, 4096).await.unwrap();
        let mut expected = vec![0x11; 4096];
        expected[2048..].fill(0x22);
        assert_eq!(
            bytes, expected,
            "the racing write must preserve all earlier acked bytes"
        );
    }

    /// Issue #204: a read must never observe stale base bytes while a flush
    /// changes its local ownership of an acked write.
    #[tokio::test]
    async fn reads_never_observe_stale_base_during_flush_handoff() {
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
        let backend = Arc::new(
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap(),
        );

        backend.write(0, &[0x11u8; 4096]).await.unwrap();

        let (arrived, proceed) = backend.arm_flush_seam(FlushSeamPoint::DirtyPendingHandoff);
        let flush_backend = backend.clone();
        let flush_task = tokio::spawn(async move { flush_backend.flush().await.map(|_| ()) });

        arrived.notified().await;

        let read_backend = backend.clone();
        let read_task = tokio::spawn(async move { read_backend.read(0, 4096).await });
        let write_backend = backend.clone();
        let write_task =
            tokio::spawn(async move { write_backend.write(2048, &[0x22u8; 2048]).await });

        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        proceed.notify_one();

        flush_task.await.unwrap().unwrap();
        let read_bytes = read_task.await.unwrap().unwrap();
        write_task.await.unwrap().unwrap();

        let before_write = vec![0x11; 4096];
        let mut expected = before_write.clone();
        expected[2048..].fill(0x22);
        assert!(
            read_bytes == before_write || read_bytes == expected,
            "a read racing the flush must return an acked disk state"
        );

        let after = backend.read(0, 4096).await.unwrap();
        assert_eq!(
            after, expected,
            "the racing write must preserve all earlier acked bytes"
        );

        backend.flush().await.unwrap();
        let published = backend.read(0, 4096).await.unwrap();
        assert_eq!(published, expected);
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
        assert_eq!(backend.dirty_chunks_count().await, 1);
        let dirty_bytes = backend.dirty_bytes().await;
        assert!(
            (16..=chunk_size).contains(&dirty_bytes),
            "dirty bytes must cover the write without exceeding one chunk"
        );

        backend.write(chunk_size, &[0xdd; 8]).await.unwrap();
        assert_eq!(backend.dirty_chunks_count().await, 2);
        let dirty_bytes = backend.dirty_bytes().await;
        assert!(
            (24..=chunk_size * 2).contains(&dirty_bytes),
            "dirty bytes must cover both writes without exceeding two chunks"
        );

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

    /// A first write below the threshold does not notify. A second write
    /// that crosses the threshold notifies once.
    #[tokio::test]
    async fn writer_crosses_threshold_pokes_notify() {
        let chunk_size = 4096u64;
        let total = chunk_size * 4;
        let manifest = synth_manifest(total, chunk_size, vec![]);
        let threshold = chunk_size * 2;
        let (backend, _store, _dir) = build_backend_with_threshold(&manifest, threshold).await;
        let notify = backend.threshold_notify();

        backend.write(0, &[0xcc; 8]).await.unwrap();
        let dirty_bytes = backend.dirty_bytes().await;
        assert!((8..=chunk_size).contains(&dirty_bytes));
        assert!(dirty_bytes < threshold);
        let below =
            tokio::time::timeout(std::time::Duration::from_millis(20), notify.notified()).await;
        assert!(
            below.is_err(),
            "notify must NOT fire before threshold is crossed",
        );

        backend.write(chunk_size, &[0xdd; 8]).await.unwrap();
        let dirty_bytes = backend.dirty_bytes().await;
        assert!((16..=chunk_size * 2).contains(&dirty_bytes));
        assert!(dirty_bytes >= threshold);
        let crossed =
            tokio::time::timeout(std::time::Duration::from_millis(100), notify.notified()).await;
        assert!(
            crossed.is_ok(),
            "notify must fire when dirty bytes cross threshold",
        );
    }

    /// Writes after the first threshold crossing do not notify again.
    #[tokio::test]
    async fn additional_writes_past_threshold_do_not_re_notify() {
        let chunk_size = 4096u64;
        let total = chunk_size * 8;
        let manifest = synth_manifest(total, chunk_size, vec![]);
        let threshold = chunk_size * 2;
        let (backend, _store, _dir) = build_backend_with_threshold(&manifest, threshold).await;
        let notify = backend.threshold_notify();

        backend.write(0, &[0xcc; 8]).await.unwrap();
        backend.write(chunk_size, &[0xdd; 8]).await.unwrap();
        let dirty_bytes = backend.dirty_bytes().await;
        assert!((16..=chunk_size * 2).contains(&dirty_bytes));
        assert!(dirty_bytes >= threshold);
        let crossed =
            tokio::time::timeout(std::time::Duration::from_millis(100), notify.notified()).await;
        assert!(crossed.is_ok(), "crossing notify must fire");

        backend.write(chunk_size * 2, &[0xee; 8]).await.unwrap();
        backend.write(chunk_size * 3, &[0xff; 8]).await.unwrap();
        let dirty_bytes = backend.dirty_bytes().await;
        assert!((32..=chunk_size * 4).contains(&dirty_bytes));
        let level =
            tokio::time::timeout(std::time::Duration::from_millis(20), notify.notified()).await;
        assert!(
            level.is_err(),
            "notify must not re-fire while the buffer stays past threshold",
        );
    }

    /// Concurrent writers produce one notification for one threshold
    /// crossing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writers_observe_single_crossing() {
        let chunk_size = 4096u64;
        let chunks: u64 = 8;
        let total = chunk_size * chunks;
        let manifest = synth_manifest(total, chunk_size, vec![]);
        let threshold = chunk_size * 4;
        let (backend, _store, _dir) = build_backend_with_threshold(&manifest, threshold).await;
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
        let dirty_bytes = backend.dirty_bytes().await;
        assert!((chunks * 8..=chunks * chunk_size).contains(&dirty_bytes));
        assert!(dirty_bytes >= threshold);

        let woken =
            tokio::time::timeout(std::time::Duration::from_millis(200), notify.notified()).await;
        assert!(
            woken.is_ok(),
            "concurrent crossing must park a notify permit",
        );

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

    /// Simulate the current migration abort sequence. The file-backed
    /// phase will no longer need to put captured bytes back into RAM.
    async fn abort_migration(backend: &ChunkedDiskBackend) -> Result<(), DiskBackendError> {
        let pending = backend.flush_local().await?;
        backend.flush_to_local_cache(&pending).await?;
        backend.requeue_pending(pending).await;
        backend.set_migration_fence(false);
        Ok(())
    }

    /// Read a disk from only its published manifest and chunk storage.
    async fn read_published_disk(
        store: Arc<ChunkStore>,
        manifest_ref: ManifestRef,
        cache_root: &std::path::Path,
        length: u64,
    ) -> Bytes {
        let mut cfg =
            ChunkCacheConfig::new(cache_root.join(format!("published-read-{}", rand_suffix())));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let restored =
            ChunkedDiskBackend::from_blob(manifest_ref, ChunkCache::new(cfg), store, u64::MAX)
                .await
                .unwrap();
        restored.read(0, length).await.unwrap()
    }

    /// ADR 0045 C1: a migration fence prevents a publish. Aborting the
    /// migration preserves every acked byte for the next flush.
    #[tokio::test]
    async fn migration_fence_prevents_publish_and_abort_preserves_bytes() {
        let chunk_size = 4096u64;
        let base = synth_manifest(chunk_size * 4, chunk_size, vec![]);
        let (backend, store, _dir) = build_backend(&base).await;
        let initial_ref = backend.manifest_ref().await;
        let mut expected = vec![0; (chunk_size * 4) as usize];
        expected[..chunk_size as usize].fill(0x42);
        backend
            .write(0, &vec![0x42; chunk_size as usize])
            .await
            .unwrap();

        backend.set_migration_fence(true);
        backend.flush().await.unwrap();
        let (latest_ref, _) = store
            .get_latest_manifest(initial_ref.manifest_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            latest_ref, initial_ref,
            "a fenced flush must not publish a new manifest"
        );
        assert_eq!(backend.read(0, chunk_size * 4).await.unwrap(), expected);

        abort_migration(&backend).await.unwrap();
        assert_eq!(backend.read(0, chunk_size * 4).await.unwrap(), expected);
        let after = backend.flush().await.unwrap();
        assert_ne!(
            after.manifest_ref, initial_ref,
            "the first unfenced flush must publish"
        );
        assert_eq!(
            read_published_disk(store, after.manifest_ref, _dir.path(), chunk_size * 4).await,
            expected,
            "the published disk must contain every acked byte"
        );
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

    /// A blob store that omits one chunk from the next manifest read.
    struct OmitChunkOnManifestReadBlob {
        inner: LocalBlobStorage,
        omit_next_manifest_read: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl BlobStorage for OmitChunkOnManifestReadBlob {
        async fn put_streaming(
            &self,
            key: &str,
            body: ByteStream,
        ) -> Result<u64, engram_core::error::BlobError> {
            self.inner.put_streaming(key, body).await
        }

        async fn get_streaming(
            &self,
            key: &str,
        ) -> Result<ByteStream, engram_core::error::BlobError> {
            if key.starts_with("manifests/")
                && self
                    .omit_next_manifest_read
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                let bytes = self.inner.get(key).await?;
                let mut manifest: Manifest = serde_json::from_slice(&bytes).map_err(|error| {
                    engram_core::error::BlobError::Protocol(format!(
                        "test manifest decode failed: {error}"
                    ))
                })?;
                manifest.chunks.pop().ok_or_else(|| {
                    engram_core::error::BlobError::Protocol(
                        "test manifest had no chunk to omit".into(),
                    )
                })?;
                let bytes = serde_json::to_vec(&manifest).map_err(|error| {
                    engram_core::error::BlobError::Protocol(format!(
                        "test manifest encode failed: {error}"
                    ))
                })?;
                return Ok(ByteStream::from_vec(bytes));
            }
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

    /// Issue #199: a fence raised during a flush prevents its publish.
    /// The bytes remain readable and a later flush can publish them.
    #[tokio::test]
    async fn fence_raised_during_flush_aborts_publish_and_preserves_bytes() {
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
        let mut expected = vec![0; total as usize];
        expected[..chunk_size as usize].fill(0x42);

        backend
            .write(0, &vec![0x42; chunk_size as usize])
            .await
            .unwrap();

        let a = {
            let backend = backend.clone();
            tokio::spawn(async move { backend.flush().await })
        };
        at_gate.notified().await;

        backend.set_migration_fence(true);

        release.notify_one();
        a.await.unwrap().unwrap();
        let (latest_ref, _) = store
            .get_latest_manifest(ref0.manifest_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            latest_ref, ref0,
            "a flush that finishes after the fence must not publish"
        );
        assert_eq!(
            backend.read(0, total).await.unwrap(),
            expected,
            "the aborted flush must retain every acked byte"
        );

        backend.set_migration_fence(false);
        let published = backend.flush().await.unwrap();
        assert_ne!(published.manifest_ref, ref0);
        assert_eq!(
            read_published_disk(store, published.manifest_ref, dir.path(), total).await,
            expected,
            "the later flush must publish every retained byte"
        );
    }

    /// ADR 0038: a failed flush does not publish a partial disk. It keeps
    /// all acked bytes readable and retries them on the next flush.
    #[tokio::test]
    async fn failed_flush_does_not_publish_and_retains_bytes() {
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
        let mut expected = vec![0; total as usize];

        for c in 0..3u64 {
            expected[(c * chunk_size) as usize..((c + 1) * chunk_size) as usize]
                .fill((c as u8) + 1);
            backend
                .write(c * chunk_size, &vec![(c as u8) + 1; chunk_size as usize])
                .await
                .unwrap();
        }

        fail.store(true, Ordering::SeqCst);
        assert!(
            backend.flush().await.is_err(),
            "flush must fail when an upload fails"
        );

        let (latest_ref, _) = store
            .get_latest_manifest(ref0.manifest_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            latest_ref, ref0,
            "a failed flush must not publish a new manifest"
        );
        assert_eq!(backend.read(0, total).await.unwrap(), expected);

        fail.store(false, Ordering::SeqCst);
        let outcome = backend.flush().await.unwrap();
        assert_ne!(
            outcome.manifest_ref, ref0,
            "a successful retry must advance the manifest"
        );
        assert_eq!(backend.read(0, total).await.unwrap(), expected);
        assert_eq!(
            read_published_disk(store, outcome.manifest_ref, dir.path(), total).await,
            expected,
            "the successful retry must publish every acked byte"
        );
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

    struct SealedSnapshot {
        seal: Arc<PostCopyDiskSeal>,
        manifest: Manifest,
        manifest_ref: ManifestRef,
    }

    struct SealedSnapshotFetcher {
        seal: Arc<PostCopyDiskSeal>,
    }

    impl PostCopyDiskFetcher for SealedSnapshotFetcher {
        fn fetch(&self, chunk_idx: u64) -> futures::future::BoxFuture<'_, Result<Bytes, String>> {
            Box::pin(async move {
                self.seal
                    .get(chunk_idx)
                    .ok_or_else(|| format!("sealed snapshot has no chunk {chunk_idx}"))
            })
        }
    }

    /// Capture a disk while a flush is in progress. The file-backed phase
    /// will replace the current local flush setup inside this helper.
    async fn seal_snapshot(
        backend: &ChunkedDiskBackend,
        writes_after_flush_starts: &[(u64, Vec<u8>)],
    ) -> SealedSnapshot {
        let interrupted_flush = backend.flush_local().await.unwrap();
        for (offset, bytes) in writes_after_flush_starts {
            backend.write(*offset, bytes).await.unwrap();
        }
        let (seal, manifest, manifest_ref) = backend.seal_for_postcopy().await;
        drop(interrupted_flush);
        SealedSnapshot {
            seal: Arc::new(seal),
            manifest,
            manifest_ref,
        }
    }

    /// Restore a captured disk after an aborted move.
    async fn restore_sealed_snapshot(backend: &ChunkedDiskBackend, snapshot: &SealedSnapshot) {
        backend.requeue_postcopy_seal(&snapshot.seal).await;
    }

    /// A source seal preserves the newest guest-visible disk and keeps its
    /// shipped manifest on the published base.
    #[tokio::test]
    async fn seal_preserves_latest_disk_view_and_base_manifest() {
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
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store.clone(), u64::MAX)
                .unwrap();

        backend
            .write(chunk_size, &vec![0x11; chunk_size as usize])
            .await
            .unwrap();
        let snapshot = seal_snapshot(
            &backend,
            &[
                (0, vec![0x22; chunk_size as usize]),
                (chunk_size, vec![0x33; 16]),
            ],
        )
        .await;
        assert_eq!(
            snapshot.manifest_ref, manifest_ref,
            "a seal must use the current published manifest"
        );

        let at = |off: u64| {
            snapshot
                .manifest
                .chunks
                .iter()
                .find(|c| c.offset == off)
                .unwrap()
                .hash
        };
        assert_eq!(at(0), h0);
        assert_eq!(at(chunk_size), h1);
        assert_eq!(at(2 * chunk_size), h2);

        let mut destination_cfg = ChunkCacheConfig::new(dir.path().join("destination-cache"));
        destination_cfg.budget_bytes = 64 * 1024 * 1024;
        let destination = ChunkedDiskBackend::new(
            manifest_ref,
            &manifest,
            ChunkCache::new(destination_cfg),
            store,
            u64::MAX,
        )
        .unwrap();
        destination
            .install_postcopy_overlay(
                &snapshot.manifest,
                snapshot.manifest_ref,
                &snapshot.seal.indices(),
                Arc::new(SealedSnapshotFetcher {
                    seal: snapshot.seal.clone(),
                }),
            )
            .await
            .unwrap();

        let mut expected = vec![0xaa; total as usize];
        expected[chunk_size as usize..(2 * chunk_size) as usize].fill(0x11);
        expected[(2 * chunk_size) as usize..].fill(0xcc);
        expected[..chunk_size as usize].fill(0x22);
        expected[chunk_size as usize..chunk_size as usize + 16].fill(0x33);
        assert_eq!(destination.read(0, total).await.unwrap(), expected);

        restore_sealed_snapshot(&backend, &snapshot).await;
        assert_eq!(backend.read(0, total).await.unwrap(), expected);
    }

    /// A sealed read fetches remote content once. A clean read uses the
    /// published base without remote traffic.
    #[tokio::test]
    async fn overlay_reads_remote_content_once_and_bypasses_clean_chunks() {
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

        let bytes = backend.read(0, chunk_size).await.unwrap();
        assert!(bytes.iter().all(|b| *b == 0xaa));
        assert_eq!(fetcher.calls(), 0);

        let bytes = backend.read(chunk_size, chunk_size).await.unwrap();
        assert!(bytes.iter().all(|b| *b == FakeFetcher::peer_byte(1)));
        assert_eq!(fetcher.calls(), 1);
        let bytes = backend.read(chunk_size, chunk_size).await.unwrap();
        assert!(bytes.iter().all(|b| *b == FakeFetcher::peer_byte(1)));
        assert_eq!(fetcher.calls(), 1, "second read must not re-fetch");
    }

    /// A partial write to remote content preserves its unwritten bytes.
    #[tokio::test]
    async fn overlay_partial_write_preserves_remote_content() {
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

    /// A terminal fetch failure makes later remote reads fail fast without
    /// another fetch.
    #[tokio::test]
    async fn overlay_fetch_failure_latches_and_fails_fast() {
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

    /// A completed drain makes the disk self-sufficient and releases its
    /// publish fence.
    #[tokio::test]
    async fn postcopy_drain_makes_disk_self_sufficient_and_unfences() {
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
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store.clone(), u64::MAX)
                .unwrap(),
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
        let outcome = backend.flush().await.unwrap();
        assert_eq!(
            outcome.chunks_flushed, 2,
            "post-drain flush publishes the pulled chunks"
        );
        let mut expected = vec![0; total as usize];
        expected[..chunk_size as usize].fill(0xaa);
        expected[chunk_size as usize..(2 * chunk_size) as usize].fill(FakeFetcher::peer_byte(1));
        expected[(2 * chunk_size) as usize..].fill(FakeFetcher::peer_byte(2));
        assert_eq!(
            read_published_disk(store, outcome.manifest_ref, dir.path(), total).await,
            expected
        );
    }

    /// A drain against a dead peer reports an error. Later reads fail
    /// without more peer traffic.
    #[tokio::test]
    async fn postcopy_drain_peer_death_reports_error_and_reads_fail_fast() {
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
        let calls_after_failure = fetcher.calls();
        assert!(backend.read(0, chunk_size).await.is_err());
        assert_eq!(
            fetcher.calls(),
            calls_after_failure,
            "a read after peer loss must fail without another fetch"
        );
    }
}
