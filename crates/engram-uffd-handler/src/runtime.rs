//! Invariant: a hot-set hint must never fail, delay, or block a move, resume,
//! drain, or blackout. Hint errors mean no hint.
//!
//! UFFD event loop. Linux-only — `userfaultfd(2)` is a Linux kernel
//! syscall and the upstream `userfaultfd` crate fails to compile on
//! other targets.
//!
//! ADR 0020 Route B — chunk-native shape (this revision):
//!
//! - There is **no `memory.bin` file and no canonical mmap.** Every
//!   page fault is served from the chunk cache/store: we consult a
//!   [`ChunkedMemoryBackend`] (see [`crate::chunked`]), `fetch_chunk`
//!   the resolved hash, and `UFFDIO_COPY` it into the guest. Zero-
//!   filled chunks install via `UFFDIO_ZEROPAGE` (no copy, kernel
//!   shared zero page, COW on write).
//! - On every fault we install a **full chunk** in one `UFFDIO_COPY`,
//!   not just the faulting page. This amortises the syscall + fault-
//!   handler cost across the chunk's pages and gets us cleanly under
//!   100 ms for typical Python workloads.
//! - As faults are served we record each chunk's position in the
//!   first-touch record, which the next restore fetches first.
//!
//! Dropping the canonical mmap is what removes the `materialize`
//! pass + the `MAP_POPULATE` eager read from the restore critical
//! path (ADR 0020). Cross-session *guest-RAM* dedup ships via the
//! ADR-0045 substrate: with `--base-shm`, canonical pages install with
//! `UFFDIO_CONTINUE` over a shared per-template base file (one host
//! copy), and only session-divergent pages stay private `UFFDIO_COPY`.
//!
//! `unsafe` blocks in this module are kernel-surface essentials
//! (UFFDIO_COPY / UFFDIO_ZEROPAGE, fd ownership from SCM_RIGHTS);
//! each is annotated with a `// SAFETY:` comment.

#![cfg(target_os = "linux")]

use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use bytes::Bytes;
use engram_chunk_store::manifest::ChunkHash;
use engram_chunk_store::FirstTouch;
use sendfd::RecvWithFd;
use tokio::runtime::Handle as TokioHandle;
use userfaultfd::{Event, Uffd};

use crate::chunked::{ChunkedBackendError, ChunkedMemoryBackend, ResolvedPage};
use crate::fault_stats::{self, Source};
use crate::proto::{GuestRegionUffdMapping, HANDSHAKE_BUF_BYTES};

/// A post-copy destination's drain, written to `prefault-stats.json` in the
/// handler's report dir once the drain is done. The live-move tests read it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
struct PeerDrainStats {
    /// Chunks the drain pulled from the peer.
    peer_pulled: u64,
    /// Chunks the peer named as equal to the durable chunk (AltSource).
    peer_alt_sourced: u64,
    /// Zero chunks the drain installed.
    peer_zero_chunks: u64,
    /// Guest faults served from the peer before the drain reached them.
    peer_live_faults: u64,
    /// Chunks installed by demand windows, including the faulting chunk.
    fault_around_chunks_installed: u64,
    /// Sealed chunks placed first by the hot-set hint.
    hot_leading: u64,
    /// A nonempty hint arrived while requests remained.
    hot_hint_received: bool,
}

/// Filename for [`PeerDrainStats`] in the handler's report dir.
pub const PREFAULT_STATS_FILE: &str = "prefault-stats.json";

/// Write the stats through a temp file and a rename, so a reader never
/// sees a partial file. Best-effort: a failure is logged, never returned.
fn write_drain_stats(path: &std::path::Path, stats: &PeerDrainStats) {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let written = serde_json::to_vec(stats)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(&tmp, bytes))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        tracing::warn!(error = %e, path = %path.display(), "drain stats write");
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Strip `O_NONBLOCK` from `fd` so blocking reads on it actually
/// block. Firecracker creates the UFFD as `O_NONBLOCK`; the
/// `userfaultfd` crate translates `EAGAIN` into `Ok(None)` and our
/// event loop's `Ok(None) => exit` arm would fire after the first
/// fault. Stripping the flag fixes that.
fn make_blocking(fd: i32) -> std::io::Result<()> {
    // SAFETY: fd is a valid kernel fd we own (received via SCM_RIGHTS,
    // wrapped in Uffd which owns it). F_GETFL / F_SETFL are
    // side-effect-free on flag bits we don't touch.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let new_flags = flags & !libc::O_NONBLOCK;
    if unsafe { libc::fcntl(fd, libc::F_SETFL, new_flags) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Anything that can go wrong during handler startup or while serving
/// faults. Kept as a unified error so `main.rs` can just bubble it.
#[derive(Debug)]
pub enum HandlerError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Uffd(userfaultfd::Error),
    Backend(ChunkedBackendError),
    /// SCM_RIGHTS payload didn't contain exactly one fd.
    UnexpectedFdCount(usize),
    /// A page fault landed at an address outside every known region.
    /// Should never happen in normal operation; if it does, our
    /// mappings drifted from Firecracker's. Surface as a hard error.
    AddressOutsideRegions(u64),
    /// The handshake's mappings sum to a size that doesn't match
    /// the canonical/session manifest's `total_bytes`. Same loud-
    /// failure reasoning — mismatched memory layout is unsafe to
    /// paper over.
    MappingSizeMismatch {
        mappings_total: u64,
        manifest_total: u64,
    },
    /// ADR 0045 substrate: the base shm file couldn't be written/probed.
    BaseShm(crate::base_shm::BaseShmError),
    /// ADR 0045 C2: the post-copy peer is gone with sealed chunks still
    /// uninstalled. FATAL by design — dirtied-since-checkpoint content
    /// has no sound second source, so the fault loop exits loud and the
    /// host-agent drives the rung-1 whole-VM rewind. Never papered over.
    PeerLost(String),
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Json(e) => write!(f, "json: {e}"),
            Self::Uffd(e) => write!(f, "uffd: {e}"),
            Self::Backend(e) => write!(f, "chunked backend: {e}"),
            Self::UnexpectedFdCount(n) => {
                write!(f, "expected exactly 1 fd via SCM_RIGHTS, got {n}")
            }
            Self::AddressOutsideRegions(addr) => {
                write!(f, "page fault at {addr:#x} outside every registered region")
            }
            Self::MappingSizeMismatch {
                mappings_total,
                manifest_total,
            } => write!(
                f,
                "FC mappings sum to {mappings_total} bytes; manifests describe \
                 {manifest_total} bytes — refusing to serve a mismatched memory layout"
            ),
            Self::BaseShm(e) => write!(f, "base shm: {e}"),
            Self::PeerLost(m) => write!(
                f,
                "post-copy peer lost with sealed chunks uninstalled: {m} \
                 (no sound second source; rung-1 rewind required)"
            ),
        }
    }
}

impl std::error::Error for HandlerError {}

impl From<std::io::Error> for HandlerError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for HandlerError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

impl From<userfaultfd::Error> for HandlerError {
    fn from(e: userfaultfd::Error) -> Self {
        Self::Uffd(e)
    }
}

impl From<ChunkedBackendError> for HandlerError {
    fn from(e: ChunkedBackendError) -> Self {
        Self::Backend(e)
    }
}

/// Receive Firecracker's handshake on `stream`: the JSON mappings
/// blob (data) plus the UFFD file descriptor (SCM_RIGHTS control msg).
/// Wraps the raw fd in a [`Uffd`].
///
/// Mirrors `UffdHandler::from_unix_stream` upstream.
pub fn recv_handshake(
    stream: &UnixStream,
) -> Result<(Vec<GuestRegionUffdMapping>, Uffd), HandlerError> {
    let mut buf = vec![0u8; HANDSHAKE_BUF_BYTES];
    let mut fds = [0i32; 1];
    let (bytes, fd_count) = stream.recv_with_fd(&mut buf, &mut fds)?;
    if fd_count != 1 {
        return Err(HandlerError::UnexpectedFdCount(fd_count));
    }
    buf.truncate(bytes);

    let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_slice(&buf)?;
    let raw_fd = fds[0];

    // SAFETY: `raw_fd` was just delivered to us by the kernel via
    // SCM_RIGHTS on an established UDS connection from Firecracker
    // which (per its source) only sends a valid userfaultfd descriptor
    // here. Ownership transfers to `Uffd`; we never touch `raw_fd`
    // again after this line.
    let uffd = unsafe { Uffd::from_raw_fd(raw_fd) };

    Ok((mappings, uffd))
}

/// Map a guest-snapshot byte offset to (mapping, host_va, room_to_eom)
/// where `host_va` is the guest's host-visible virtual address the
/// faulting page lives at, and `room_to_eom` is how many bytes from
/// `host_va` are still inside this mapping. Returns `None` when the
/// offset isn't covered by any mapping.
///
/// Every byte-offset translation funnels through here.
fn locate_offset(
    mappings: &[GuestRegionUffdMapping],
    byte_offset: u64,
) -> Option<(&GuestRegionUffdMapping, u64, u64)> {
    for m in mappings {
        let m_offset = m.offset;
        let m_size = m.size as u64;
        if byte_offset >= m_offset && byte_offset < m_offset + m_size {
            let intra = byte_offset - m_offset;
            let host_va = m.base_host_virt_addr + intra;
            let room = m_size - intra;
            return Some((m, host_va, room));
        }
    }
    None
}

/// Per-chunk installation lifecycle. A chunk's `installed` bit was
/// historically a plain `bool` set *before* the fallible fetch/copy
/// work — so a transient error (GCS 5xx, partial `UFFDIO_COPY`) left
/// the bit set with no rollback, permanently poisoning the chunk: the
/// next fault saw "installed", `wake_page`d a still-missing page, and
/// the vCPU spun in a fault/wake loop forever (issue #206).
///
/// The tri-state makes install atomic *and* serialised:
/// - `Empty`      — no install has succeeded; the chunk is claimable.
/// - `Installing` — exactly one installer holds the claim and is doing
///   the fallible work (which can block multi-seconds on `fetch_chunk`).
/// - `Installed`  — the bytes are in the guest; durable success.
///
/// `Empty → Installing` is the claim (CAS under the lock). Success
/// publishes `Installing → Installed`; any error rolls back
/// `Installing → Empty` so the chunk is retried on the next fault. A
/// concurrent fault that finds `Installing` *waits* for the in-flight
/// installer to publish a terminal state (closing the busy-spin
/// livelock window) instead of waking a page that isn't there yet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChunkState {
    Empty,
    Installing,
    Installed,
}

/// Result of attempting to claim a chunk for installation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Claim {
    /// WE claimed it (state was `Empty`, now `Installing`): the caller
    /// must perform the install and then `mark_installed` / `release_chunk`.
    Claimed,
    /// Another installer is mid-flight (`Installing`). The caller should
    /// wait for it to finish (`wait_until_settled`) rather than wake a
    /// missing page.
    InFlight,
    /// Already fully `Installed` — the caller short-circuits to a wake.
    Done,
}

/// Drain connections for a large drain.
const DRAIN_CONNS: usize = 3;
/// A drain of this many chunks or fewer uses one connection.
const DRAIN_SPLIT_MIN_CHUNKS: usize = 64;
/// Requests each drain connection keeps on the wire.
const PIPELINE_DEPTH: usize = 8;
/// Requests each drain connection keeps on the wire while a guest fault
/// is in flight or finished within [`FAULT_RECENT`].
const FAULT_SHARE_DEPTH: usize = 2;
const FAULT_RECENT: Duration = Duration::from_millis(2);
/// How long a fault (or the drain) waits for another request's install
/// of the same chunk before it asks for the chunk itself.
const PEER_REQUESTED_WAIT: Duration = Duration::from_millis(50);

/// The most time the eager sweep runs. Each install is whole, so the sweep
/// can stop between chunks; the rest serve on demand. This bound keeps a
/// slow sweep from holding the I/O long after it helps.
const SWEEP_BUDGET: Duration = Duration::from_secs(60);

/// How long the eager sweep waits before it goes over the chunks it could
/// not install yet.
const SWEEP_RETRY: Duration = Duration::from_millis(250);

/// Passes over the waiting chunks in which none lands before the sweep
/// stops (5 s). A running prefetch lands hundreds of chunks a second.
const SWEEP_STALL_PASSES: usize = 20;

/// How a chunk installs. [`Runtime::fill_for`] takes this decision for an
/// offset from the manifests; a peer response gives it directly.
enum Fill {
    /// Bytes private to this session: copied in (`UFFDIO_COPY`).
    Private(Payload),
    /// The base image's chunk: mapped from the shared base file
    /// (`UFFDIO_CONTINUE`) when there is one, else copied in.
    Canonical(Payload),
    /// A zero chunk (`UFFDIO_ZEROPAGE`).
    Zero,
}

/// A chunk's bytes, or the store chunk that holds them.
enum Payload {
    Hash(ChunkHash),
    Bytes(Bytes),
}

impl Payload {
    fn bytes(
        self,
        backend: &ChunkedMemoryBackend,
        handle: &TokioHandle,
    ) -> Result<Bytes, HandlerError> {
        match self {
            Payload::Hash(hash) => Ok(backend.fetch_chunk(handle, hash)?),
            Payload::Bytes(bytes) => Ok(bytes),
        }
    }

    /// Whether the bytes are at hand: given, or in the local cache.
    fn is_resident(&self, backend: &ChunkedMemoryBackend) -> bool {
        match self {
            Payload::Hash(hash) => backend.is_resident(*hash),
            Payload::Bytes(_) => true,
        }
    }
}

impl Fill {
    /// The same fill, with its bytes fetched now.
    fn fetched(
        self,
        backend: &ChunkedMemoryBackend,
        handle: &TokioHandle,
    ) -> Result<Self, HandlerError> {
        Ok(match self {
            Fill::Private(p) => Fill::Private(Payload::Bytes(p.bytes(backend, handle)?)),
            Fill::Canonical(p) => Fill::Canonical(Payload::Bytes(p.bytes(backend, handle)?)),
            Fill::Zero => Fill::Zero,
        })
    }
}

/// Who installs a chunk. A fault wakes the vCPUs that wait on it; a peer
/// install is recorded in the sealed output.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Installer {
    Fault,
    Sweep,
    Peer,
}

/// What [`Runtime::sweep_step`] did with one chunk.
enum SweepStep {
    Installed,
    /// Its bytes are not at hand yet.
    Wait,
    /// A zero chunk, or a canonical one with no hash: faults serve it.
    Zero,
    /// Installed already, sealed, left to the guest, or past the end.
    Skip,
}

/// The work list the drain connections share.
struct DrainShared {
    /// `(offset, forced)`: a forced chunk is requested even when another
    /// request holds it.
    todo: Mutex<std::collections::VecDeque<(u64, bool)>>,
    /// Chunks skipped because another request held them.
    deferred: Mutex<Vec<u64>>,
    failed: AtomicBool,
    /// Distinct from fault-connection request ids in logs.
    next_req: std::sync::atomic::AtomicU64,
    resolved: std::sync::atomic::AtomicUsize,
    total: usize,
    pulled: std::sync::atomic::AtomicU64,
    alt_sourced: std::sync::atomic::AtomicU64,
    zero_chunks: std::sync::atomic::AtomicU64,
}

impl DrainShared {
    fn new(todo: Vec<u64>) -> Self {
        Self {
            total: todo.len(),
            todo: Mutex::new(todo.into_iter().map(|o| (o, false)).collect()),
            deferred: Mutex::new(Vec::new()),
            failed: AtomicBool::new(false),
            next_req: std::sync::atomic::AtomicU64::new(1_000_000),
            resolved: std::sync::atomic::AtomicUsize::new(0),
            pulled: std::sync::atomic::AtomicU64::new(0),
            alt_sourced: std::sync::atomic::AtomicU64::new(0),
            zero_chunks: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn pop(&self) -> Option<(u64, bool)> {
        self.todo.lock().expect("drain queue poisoned").pop_front()
    }

    fn defer(&self, offset: u64) {
        self.deferred
            .lock()
            .expect("drain queue poisoned")
            .push(offset);
    }

    fn force(&self, offset: u64) {
        self.todo
            .lock()
            .expect("drain queue poisoned")
            .push_front((offset, true));
    }

    /// Count one chunk as installed, and report progress every 256.
    fn resolved(&self, rt: &Runtime) {
        let n = self.resolved.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(256) {
            if let Some(control) = rt.control.as_ref() {
                control.report(engram_migrate_proto::HandlerControl::DrainProgress {
                    pulled: self.pulled.load(Ordering::Relaxed),
                    remaining: self.total.saturating_sub(n) as u64,
                });
            }
        }
    }
}

/// One bounded prefetch slot. Requests are never queued behind a window.
struct FaultWindow {
    progress: Arc<WindowProgress>,
    thread: std::thread::JoinHandle<()>,
}

struct WindowProgress {
    stopped: AtomicBool,
    // Serializes the final cancellation check with installs and report publication.
    socket: Mutex<Option<std::net::TcpStream>>,
}

impl WindowProgress {
    fn new() -> Self {
        Self {
            stopped: AtomicBool::new(false),
            socket: Mutex::new(None),
        }
    }

    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
}

impl FaultWindow {
    fn stop(self) {
        let deadline = std::time::Instant::now() + crate::peer::PEER_READ_TIMEOUT;
        loop {
            match self.progress.socket.try_lock() {
                Ok(socket) => {
                    self.progress.stopped.store(true, Ordering::SeqCst);
                    if let Some(socket) = &*socket {
                        let _ = socket.shutdown(std::net::Shutdown::Both);
                    }
                    break;
                }
                Err(std::sync::TryLockError::Poisoned(_)) => panic!("window poisoned"),
                Err(std::sync::TryLockError::WouldBlock) => {}
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                self.progress.stopped.store(true, Ordering::SeqCst);
                tracing::warn!("fault-around install lock missed stop deadline; detaching");
                return;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(5)));
        }
        while !self.thread.is_finished() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!("fault-around worker did not stop before deadline; detaching");
                return;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(5)));
        }
        let _ = self.thread.join();
    }
}

/// All the state the event loop needs after the handshake.
pub struct Runtime {
    mappings: Vec<GuestRegionUffdMapping>,
    uffd: Uffd,
    backend: Arc<ChunkedMemoryBackend>,
    /// Tokio handle used by the (sync) fault loop to drive
    /// async chunk fetches via `Handle::block_on`. The fault loop
    /// runs on a `spawn_blocking` thread which is safe to call
    /// `block_on` from.
    handle: TokioHandle,
    /// Per-chunk installation lifecycle (see [`ChunkState`]). Lets the
    /// fault loop skip duplicate `UFFDIO_COPY` attempts for chunks
    /// already installed (`EEXIST` would otherwise force a retry), and
    /// — via the tri-state + `install_cv` — lets a fault that races an
    /// in-flight install *wait* for it instead of waking a missing page.
    /// A failed install rolls the chunk back to `Empty` so it is retried
    /// rather than permanently poisoned (issue #206).
    installed: Mutex<Vec<ChunkState>>,
    /// The chunks the guest needs first, by position, for the next restore's prefix.
    first_touch: crate::first_touch::FirstTouchRecorder,
    /// Notified whenever a chunk leaves `Installing` (→ `Installed` on
    /// success, → `Empty` on rollback). A fault that found the chunk
    /// `Installing` waits on this until the in-flight install settles.
    install_cv: Condvar,
    page_size: u64,
    /// Where the drain stats go (the jail dir).
    report_dir: Option<PathBuf>,
    /// ADR 0045 unified memory substrate (v2b). When set, guest memory
    /// is `MAP_PRIVATE` of this per-template base shm file (registered
    /// `MISSING|MINOR` by the forked FC) and canonical chunks install
    /// via populate-the-base + `UFFDIO_CONTINUE` — one page-cache copy
    /// shared by every same-template VM on the host — instead of a
    /// private `UFFDIO_COPY`. Session-divergent chunks keep COPY.
    /// `None` ⇒ stock Route-B behavior, byte-identical.
    base_shm: Option<crate::base_shm::BaseShm>,
    /// Whether `UFFDIO_ZEROPAGE` works on the guest mapping (the
    /// substrate's MAP_PRIVATE file mapping depends on the kernel).
    /// Starts `true`; the first EINVAL flips it, and zero chunks then
    /// install copies of a zeroed buffer.
    zeropage_ok: std::sync::atomic::AtomicBool,
    /// ADR 0045 C2: post-copy migration mode. Sealed (dirtied-since-
    /// checkpoint) chunks are peer-authoritative: the fault path asks
    /// the source's page server instead of `resolve()`, and the drain
    /// task pulls the rest in the background. `None` ⇒ everything
    /// above is independent of the source.
    peer: Option<std::sync::Arc<crate::peer::PeerSession>>,
    /// Peer mode: one flag per chunk, set while a request for the chunk
    /// is on the wire (drain, fault window, or direct demand). The drain
    /// skips a requested chunk and a fault waits for its install, so a
    /// chunk is asked for once. Empty outside peer mode.
    peer_requested: Vec<AtomicBool>,
    /// How long a fault or the drain waits for another request's install
    /// of a chunk before it asks for the chunk itself.
    peer_requested_wait: Duration,
    /// Peer mode: where every peer-supplied chunk is recorded.
    sealed_out: Option<crate::peer::SealedOutput>,
    fault_window: Mutex<Option<FaultWindow>>,
    fault_around_chunks: usize,
    fault_around_chunks_installed: std::sync::atomic::AtomicU64,
    /// One-way progress/failure reports to the host-agent (peer mode).
    control: Option<std::sync::Arc<crate::peer::ControlTx>>,
    /// ADR 0045 C2: latched once the background drain has pulled every
    /// sealed chunk (DrainDone). After this, the destination holds all
    /// sealed content locally and no longer needs the source — the
    /// fault path must NOT dial the peer (the source's export may already
    /// be torn down by `migration_commit`). A fault that finds its chunk
    /// installed simply wakes; a (vanishingly unlikely) still-uninstalled
    /// sealed chunk after a successful drain falls through to `resolve()`
    /// rather than fatally escalating PeerLost on a healthy, fully-drained
    /// destination (issue #227 scenario (a)).
    drain_done: std::sync::atomic::AtomicBool,
}

impl Runtime {
    /// Pair the chunked backend with the UFFD the handshake gave us.
    /// No `memory.bin` file is opened — every page is served from
    /// chunks (ADR 0020 Route B).
    pub fn new(
        mappings: Vec<GuestRegionUffdMapping>,
        uffd: Uffd,
        backend: Arc<ChunkedMemoryBackend>,
        handle: TokioHandle,
        base_shm: Option<crate::base_shm::BaseShm>,
    ) -> Result<Self, HandlerError> {
        let mappings_total: u64 = mappings.iter().map(|m| m.size as u64).sum();
        if mappings_total != backend.total_bytes() {
            return Err(HandlerError::MappingSizeMismatch {
                mappings_total,
                manifest_total: backend.total_bytes(),
            });
        }

        // `page_size` is whatever the registered region reported.
        // FC carves the guest's address space into 4 KiB pages on
        // x86 and 16 KiB on aarch64; per-region the kernel guarantees
        // this is consistent.
        let page_size = mappings.first().map(|m| m.page_size as u64).unwrap_or(4096);
        let chunk_size = backend.chunk_size();
        let chunk_count = backend.total_bytes().div_ceil(chunk_size) as usize;

        // A chunk that straddles a region boundary must install across
        // both regions (`install_spanning`). Misaligned boundaries are
        // expected on FC x86 (the BIOS hole at 640 KiB is not 512 KiB-
        // chunk-aligned), so this is a visibility breadcrumb, not an
        // error — but it's the precise condition the spanning install
        // exists to handle, so flag it if a future layout introduces one.
        for w in mappings.windows(2) {
            let boundary = w[0].offset + w[0].size as u64;
            if !boundary.is_multiple_of(chunk_size) {
                tracing::debug!(
                    boundary,
                    chunk_size,
                    "region boundary is not chunk-aligned; chunks spanning it \
                     install across regions via install_spanning"
                );
            }
        }

        Ok(Self {
            mappings,
            uffd,
            backend,
            handle,
            installed: Mutex::new(vec![ChunkState::Empty; chunk_count]),
            first_touch: crate::first_touch::FirstTouchRecorder::new(chunk_count),
            install_cv: Condvar::new(),
            page_size,
            report_dir: None,
            base_shm,
            zeropage_ok: std::sync::atomic::AtomicBool::new(true),
            peer: None,
            peer_requested: Vec::new(),
            peer_requested_wait: PEER_REQUESTED_WAIT,
            sealed_out: None,
            fault_window: Mutex::new(None),
            fault_around_chunks: crate::peer::DEFAULT_FAULT_AROUND_CHUNKS,
            fault_around_chunks_installed: std::sync::atomic::AtomicU64::new(0),
            control: None,
            drain_done: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// ADR 0045 C2: arm post-copy mode. The session already holds the
    /// seal (connect blocks on it), so the fault path can classify from
    /// the first fault.
    pub fn set_peer(
        &mut self,
        peer: std::sync::Arc<crate::peer::PeerSession>,
        control: Option<std::sync::Arc<crate::peer::ControlTx>>,
    ) {
        let chunk_count = self
            .installed
            .lock()
            .expect("installed bitmap poisoned")
            .len();
        self.peer_requested = (0..chunk_count).map(|_| AtomicBool::new(false)).collect();
        self.peer = Some(peer);
        self.control = control;
    }

    /// Peer mode: record every peer-supplied chunk in `out`.
    pub fn set_sealed_output(&mut self, out: crate::peer::SealedOutput) {
        self.sealed_out = Some(out);
    }

    /// Mark `chunk_idx` requested. `false` when a request is already out.
    fn try_request(&self, chunk_idx: usize) -> bool {
        self.peer_requested
            .get(chunk_idx)
            .is_some_and(|f| !f.swap(true, Ordering::AcqRel))
    }

    fn is_requested(&self, chunk_idx: usize) -> bool {
        self.peer_requested
            .get(chunk_idx)
            .is_some_and(|f| f.load(Ordering::Acquire))
    }

    /// Clear the request flag and wake faults that wait on the chunk.
    fn clear_request(&self, chunk_idx: usize) {
        if let Some(flag) = self.peer_requested.get(chunk_idx) {
            if flag.swap(false, Ordering::AcqRel) {
                let _installed = self.installed.lock().expect("installed bitmap poisoned");
                self.install_cv.notify_all();
            }
        }
    }

    /// Wait up to `peer_requested_wait` for a requested chunk to install.
    /// `true` when it is installed; `false` when the request ended
    /// without an install, or the bound passed.
    fn await_requested(&self, chunk_idx: usize) -> bool {
        let deadline = std::time::Instant::now() + self.peer_requested_wait;
        let mut installed = self.installed.lock().expect("installed bitmap poisoned");
        loop {
            if installed.get(chunk_idx) == Some(&ChunkState::Installed) {
                return true;
            }
            if !self.is_requested(chunk_idx) {
                return false;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            installed = self
                .install_cv
                .wait_timeout(installed, remaining)
                .expect("installed bitmap poisoned")
                .0;
        }
    }

    /// Write the first-touch record (as each entry is recorded) and the
    /// drain stats to `dir`.
    pub fn set_report_dir(&mut self, dir: PathBuf) {
        self.first_touch.set_output(&dir.join(FirstTouch::FILE));
        self.report_dir = Some(dir);
    }

    /// Log the memory manifest's composition once.
    fn log_composition(&self) {
        let c = self.backend.composition();
        let canonical_in_shm = self.base_shm.as_ref().map(|base| {
            let chunk_size = self.backend.chunk_size();
            let total = self.backend.total_bytes();
            (0..total.div_ceil(chunk_size))
                .map(|i| i * chunk_size)
                .filter(|&off| {
                    matches!(
                        self.backend.resolve(off),
                        Some(ResolvedPage::Canonical { .. })
                    ) && base.is_populated(off, chunk_size.min(total - off))
                })
                .count()
        });
        tracing::info!(
            canonical = c.canonical,
            canonical_in_shm,
            divergent = c.divergent,
            divergent_local = c.divergent_local,
            zero = c.zero,
            chunk_size = self.backend.chunk_size(),
            "memory manifest composition"
        );
    }

    /// ADR 0045 tail latency: eagerly install EVERY chunk of the
    /// session — canonical via the shared base (CONTINUE), divergent
    /// via fetch + COPY — as a background producer racing the fault
    /// loop. Before this sweep a migration restore's divergent chunks
    /// (the checkpoint chain's accumulated diff — hundreds of 512 KiB
    /// chunks, all prestaged on local NVMe) faulted in one at a time
    /// through the single-threaded fault loop: the residual ~27 s
    /// post-teleport guest crawl. Idempotent
    /// against the fault loop via the `installed` bitmap; `wake=false`
    /// (no vCPU waits on a page it hasn't faulted).
    ///
    /// With a writer, the sweep never fetches: it installs only chunks
    /// the cache (or the base shm) already holds. It keeps the chunks it
    /// could not install yet and goes over only those every
    /// [`SWEEP_RETRY`] as the writer's prefetch lands them. After
    /// [`SWEEP_STALL_PASSES`] passes in which none landed (the prefetch
    /// ended or the writer restarted), it stops and leaves them to the
    /// faults. A fetch here would be a `Demand` request, made one at a
    /// time on this thread, for a chunk the prefetch has not reached yet.
    /// Without a writer nothing else fills the cache, so the sweep
    /// fetches.
    pub fn sweep_all(&self) -> Result<(), HandlerError> {
        let started = std::time::Instant::now();
        let chunk_size = self.backend.chunk_size();
        let total = self.backend.total_bytes();
        let follow = self.backend.has_writer();
        let mut installed = 0usize;
        let mut zero_skipped = 0usize;
        // Divergent first: those are the chunks an on-demand fault pays a
        // fetch for (the post-teleport crawl); canonical pages CONTINUE
        // from the (pre-warmed) base for near-free, and ZERO chunks are
        // skipped entirely — eagerly installing them would privately
        // materialize every untouched page of guest RAM
        // (UFFDIO_ZEROPAGE is rejected on the substrate's MAP_PRIVATE
        // file mapping, so each would COPY a zeroed buffer), defeating
        // the density the substrate exists for. A zero fault is already
        // served with no fetch.
        let (mut order, canonical): (Vec<u64>, Vec<u64>) = (0..total)
            .step_by(chunk_size as usize)
            .partition(|&offset| {
                matches!(
                    Self::fill_for(&self.backend, offset),
                    Some(Fill::Private(_))
                )
            });
        order.extend(canonical);
        let mut pending = Vec::new();
        for offset in order {
            match self.sweep_step(offset, follow)? {
                SweepStep::Installed => installed += 1,
                SweepStep::Zero => zero_skipped += 1,
                SweepStep::Wait => pending.push(offset),
                SweepStep::Skip => {}
            }
        }
        let mut passes = 1usize;
        let mut stalled = 0usize;
        let end = loop {
            if pending.is_empty() {
                break "complete";
            }
            if started.elapsed() > SWEEP_BUDGET {
                break "budget";
            }
            if stalled >= SWEEP_STALL_PASSES {
                break "stalled";
            }
            std::thread::sleep(SWEEP_RETRY);
            let before = installed;
            let mut still = Vec::with_capacity(pending.len());
            for offset in pending {
                match self.sweep_step(offset, follow)? {
                    SweepStep::Installed => installed += 1,
                    SweepStep::Wait => still.push(offset),
                    SweepStep::Zero | SweepStep::Skip => {}
                }
            }
            pending = still;
            passes += 1;
            stalled = if installed > before { 0 } else { stalled + 1 };
        };
        let (carried, carried_touched) = self.first_touch.carried_use();
        tracing::info!(
            end,
            installed,
            carried,
            carried_touched,
            passes,
            left_to_faults = pending.len(),
            zero_skipped,
            elapsed_ms = started.elapsed().as_millis() as u64,
            total_chunks = total.div_ceil(chunk_size),
            "eager sweep complete (ADR 0045)"
        );
        Ok(())
    }

    /// One chunk of the eager sweep: install it when its bytes are at
    /// hand (with a writer, `follow`: when the cache or the base shm
    /// holds them).
    fn sweep_step(&self, offset: u64, follow: bool) -> Result<SweepStep, HandlerError> {
        let idx = self.chunk_idx(offset);
        // ADR 0045 C2: a sealed-but-undrained chunk's truth is the PEER,
        // not the manifest — installing resolve()'s stale view here would
        // be silent corruption. The drain runs before this on the
        // producer thread, so normally every sealed chunk is already
        // installed; this guard is the belt-and-braces for any other
        // interleaving.
        let sealed = self
            .peer
            .as_ref()
            .is_some_and(|peer| peer.seal().get(idx as u64));
        if self.chunk_installed(idx) || sealed {
            return Ok(SweepStep::Skip);
        }
        // The guest's own fault records a carried chunk in the next
        // prefix; an install here would hide it.
        if self.first_touch.awaits_guest(idx) {
            return Ok(SweepStep::Skip);
        }
        let Some(fill) = Self::fill_for(&self.backend, offset) else {
            return Ok(SweepStep::Skip);
        };
        let held = match &fill {
            Fill::Zero => return Ok(SweepStep::Zero),
            Fill::Private(payload) => payload.is_resident(&self.backend),
            Fill::Canonical(payload) => {
                payload.is_resident(&self.backend)
                    || self
                        .base_shm
                        .as_ref()
                        .is_some_and(|base| base.is_populated(offset, self.chunk_len(offset)))
            }
        };
        if follow && !held {
            return Ok(SweepStep::Wait);
        }
        Ok(if self.install(offset, fill, Installer::Sweep)? {
            SweepStep::Installed
        } else {
            SweepStep::Skip
        })
    }

    /// Atomically claim a chunk for installation (CAS `Empty →
    /// Installing` under the lock). Concurrent sweep / fault / drain
    /// installs serialise to exactly one installer per chunk.
    ///
    /// - [`Claim::Claimed`]  — WE own the install; on success call
    ///   `mark_installed`, on ANY error call `release_chunk` so the
    ///   chunk rolls back to `Empty` and stays retryable (issue #206:
    ///   the bit is never left set across a failed fetch/copy).
    /// - [`Claim::InFlight`] — someone else is installing; the caller
    ///   should `wait_until_settled` rather than wake a missing page.
    /// - [`Claim::Done`]     — already `Installed`; caller wakes.
    fn claim_chunk(&self, chunk_idx: usize) -> Claim {
        let mut installed = self.installed.lock().expect("installed bitmap poisoned");
        match installed.get_mut(chunk_idx) {
            Some(slot @ ChunkState::Empty) => {
                *slot = ChunkState::Installing;
                Claim::Claimed
            }
            Some(ChunkState::Installing) => Claim::InFlight,
            Some(ChunkState::Installed) => Claim::Done,
            // Out-of-range index: treat as "claimed" so the caller
            // proceeds and the subsequent locate_offset surfaces the
            // real out-of-bounds error rather than silently looping.
            None => Claim::Claimed,
        }
    }

    /// Publish a successful install (`Installing → Installed`) and wake
    /// any faults waiting on the in-flight install. The bytes are in
    /// the guest before this is called, so it is sound for a woken
    /// waiter to `wake_page` its faulting page.
    fn mark_installed(&self, chunk_idx: usize) {
        let mut installed = self.installed.lock().expect("installed bitmap poisoned");
        if let Some(slot) = installed.get_mut(chunk_idx) {
            *slot = ChunkState::Installed;
        }
        self.install_cv.notify_all();
    }

    /// Roll back a failed install (`Installing → Empty`) so the chunk
    /// can be re-claimed and retried, and wake any waiting faults so
    /// one of them re-claims and retries the fetch/copy instead of
    /// spinning (closes the bit-set-before-install permanent-loss window
    /// AND the in-flight livelock window — issue #206).
    fn release_chunk(&self, chunk_idx: usize) {
        let mut installed = self.installed.lock().expect("installed bitmap poisoned");
        if let Some(slot) = installed.get_mut(chunk_idx) {
            *slot = ChunkState::Empty;
        }
        self.install_cv.notify_all();
    }

    /// Block until `chunk_idx` is no longer `Installing` (an in-flight
    /// installer published `Installed` or rolled back to `Empty`).
    /// Returns the settled state. Used by a fault that raced a producer
    /// install: it waits for the result instead of waking a page the
    /// in-flight install hasn't populated yet. The wait is bounded only
    /// by the installer's own work (a `fetch_chunk` is cache/store-
    /// bounded); on `Empty` the caller retries, on `Installed` it wakes.
    fn wait_until_settled(&self, chunk_idx: usize) -> ChunkState {
        let mut installed = self.installed.lock().expect("installed bitmap poisoned");
        loop {
            match installed.get(chunk_idx).copied() {
                Some(ChunkState::Installing) => {
                    installed = self
                        .install_cv
                        .wait(installed)
                        .expect("installed bitmap poisoned");
                }
                Some(state) => return state,
                None => return ChunkState::Installed,
            }
        }
    }

    /// Claim a chunk for the calling installer, resolving an in-flight
    /// race deterministically. Returns either [`Claim::Claimed`] (WE own
    /// the install; settle it with `settle_install`) or [`Claim::Done`]
    /// (the chunk is `Installed`; the caller wakes its faulting page).
    ///
    /// On [`Claim::InFlight`] we WAIT for the racing installer to settle
    /// rather than wake a page it hasn't populated (the old busy-spin
    /// livelock, issue #206). If that install succeeded → `Done`; if it
    /// failed (rolled back to `Empty`) we re-claim and retry ourselves,
    /// so a transient producer error is repaired by the next fault
    /// instead of permanently poisoning the chunk.
    fn claim_for_install(&self, chunk_idx: usize) -> Claim {
        loop {
            match self.claim_chunk(chunk_idx) {
                Claim::Claimed => return Claim::Claimed,
                Claim::Done => return Claim::Done,
                Claim::InFlight => match self.wait_until_settled(chunk_idx) {
                    // Racing install landed the bytes; wake our page.
                    ChunkState::Installed => return Claim::Done,
                    // Racing install failed and rolled back; loop to
                    // re-claim and retry the fetch/copy ourselves.
                    ChunkState::Empty | ChunkState::Installing => continue,
                },
            }
        }
    }

    /// Publish the outcome of an install WE claimed: `Installed` on
    /// success, rollback to `Empty` on any error (so `?`-propagated
    /// errors can never leave the chunk stuck `Installing`).
    fn settle_install<T>(&self, chunk_idx: usize, result: &Result<T, HandlerError>) {
        match result {
            Ok(_) => self.mark_installed(chunk_idx),
            Err(_) => self.release_chunk(chunk_idx),
        }
    }

    /// Smallest mapped file offset `>= from` across all regions, or
    /// `None` if no region covers any offset at or after `from`. Used
    /// to skip inter-region file gaps when walking a chunk that the
    /// mappings don't cover contiguously.
    fn next_mapped_offset(&self, from: u64) -> Option<u64> {
        self.mappings
            .iter()
            .filter_map(|m| {
                let end = m.offset + m.size as u64;
                if from < end {
                    Some(m.offset.max(from))
                } else {
                    None
                }
            })
            .min()
    }

    /// Install a chunk's worth of bytes over the file-offset range
    /// `[byte_offset, byte_offset + total_len)`, walking *every* region
    /// the range touches.
    ///
    /// A chunk can straddle an FC memory-region boundary (FC x86 splits
    /// guest RAM around the BIOS hole at 640 KiB, which is not aligned
    /// to the 512 KiB memory chunk size). A single `locate_offset` only
    /// finds the region containing the chunk *start*, so clamping to
    /// that region's tail leaves the spillover into the next region(s)
    /// permanently uninstalled — and since the per-chunk `installed` bit
    /// would be set, no later fault ever repairs it (the fault path
    /// rounds down to the same chunk start, short-circuits on the bit,
    /// and `wake_page`s a still-missing page → fault/wake livelock).
    ///
    /// For each region segment this resolves `(host_va, room_to_eom)`
    /// via `locate_offset` and calls `install_seg(host_va, seg_len)` —
    /// the caller supplies the actual ioctl (COPY / ZEROPAGE / CONTINUE)
    /// and the source-byte slicing keyed off the in-chunk offset
    /// (passed as `chunk_pos`). File ranges genuinely absent from the
    /// mappings (inter-region gaps) are skipped.
    ///
    /// The caller is responsible for setting the per-chunk `installed`
    /// bit only *after* this returns `Ok`, so a mid-walk error leaves
    /// the bit clear and the chunk retryable.
    fn install_spanning<F>(
        &self,
        byte_offset: u64,
        total_len: u64,
        mut install_seg: F,
    ) -> Result<(), HandlerError>
    where
        // (host_va, segment_len, chunk_pos) -> Result<(), _>
        // `chunk_pos` is the offset of this segment from `byte_offset`
        // (i.e. where in the chunk's source buffer the bytes start).
        F: FnMut(u64, usize, usize) -> Result<(), HandlerError>,
    {
        let end = byte_offset + total_len;
        let mut cursor = byte_offset;
        while cursor < end {
            let Some((_m, host_va, room_to_eom)) = locate_offset(&self.mappings, cursor) else {
                // `cursor` falls in an inter-region gap (a file range no
                // mapping covers). Skip ahead to the next mapped offset
                // still inside this chunk; if there is none, we're done.
                match self.next_mapped_offset(cursor) {
                    Some(next) if next < end => {
                        cursor = next;
                        continue;
                    }
                    _ => break,
                }
            };
            let remaining = end - cursor;
            let seg_len = std::cmp::min(remaining, room_to_eom) as usize;
            debug_assert!(
                seg_len.is_multiple_of(self.page_size as usize),
                "segment install length must be page-aligned"
            );
            let chunk_pos = (cursor - byte_offset) as usize;
            install_seg(host_va, seg_len, chunk_pos)?;
            cursor += seg_len as u64;
        }
        Ok(())
    }

    fn chunk_idx(&self, offset: u64) -> usize {
        (offset / self.backend.chunk_size()) as usize
    }

    /// The length of the chunk at `offset`: the last chunk can be short.
    fn chunk_len(&self, offset: u64) -> u64 {
        self.backend
            .chunk_size()
            .min(self.backend.total_bytes().saturating_sub(offset))
    }

    /// How the chunk at `offset` installs, from the manifests: the one
    /// source decision every installer takes. `None` past the end.
    fn fill_for(backend: &ChunkedMemoryBackend, offset: u64) -> Option<Fill> {
        Some(match backend.resolve(offset)? {
            ResolvedPage::Chunk { hash } => Fill::Private(Payload::Hash(hash)),
            ResolvedPage::Canonical { hash } => Fill::Canonical(Payload::Hash(hash)),
            ResolvedPage::Zero => Fill::Zero,
        })
    }

    /// Install the chunk at `offset` from `fill`. `Ok(false)` when another
    /// installer had it. The claim is held across the fetch, so a chunk is
    /// fetched once however many installers race for it.
    fn install(&self, offset: u64, fill: Fill, by: Installer) -> Result<bool, HandlerError> {
        if self.claim_for_install(self.chunk_idx(offset)) != Claim::Claimed {
            return Ok(false);
        }
        self.install_claimed(offset, fill, by)
    }

    /// Install a chunk the caller claimed, then settle the claim:
    /// `Installed` on success, back to `Empty` on any error, so a failed
    /// fetch or copy leaves the chunk retryable (#206). A peer install is
    /// recorded before it is marked installed, so once every sealed chunk
    /// is installed the sealed output is complete.
    fn install_claimed(
        &self,
        offset: u64,
        fill: Fill,
        by: Installer,
    ) -> Result<bool, HandlerError> {
        let len = self.chunk_len(offset);
        let record = self.sealed_out.as_ref().filter(|_| by == Installer::Peer);
        let wake = by == Installer::Fault;
        let result = (|| {
            let base = match fill {
                Fill::Canonical(_) => self.base_shm.as_ref(),
                _ => None,
            };
            // A base that holds the chunk needs no bytes, unless they are recorded.
            let bytes = match fill {
                Fill::Zero => None,
                Fill::Canonical(_)
                    if record.is_none() && base.is_some_and(|b| b.is_populated(offset, len)) =>
                {
                    None
                }
                Fill::Private(payload) | Fill::Canonical(payload) => {
                    Some(payload.bytes(&self.backend, &self.handle)?)
                }
            };
            if let Some(out) = record {
                out.record(offset, bytes.as_deref(), len);
            }
            match (base, bytes) {
                (Some(base), bytes) => self.continue_from_base(base, offset, len, bytes, wake),
                (None, Some(bytes)) => self.copy_in(offset, &bytes, wake),
                (None, None) => self.zero_in(offset, len, wake),
            }
        })();
        self.settle_install(self.chunk_idx(offset), &result);
        result.map(|()| true)
    }

    /// `UFFDIO_COPY` `bytes` (a full chunk, or the short last one) into
    /// every region the chunk spans (see `install_spanning`).
    fn copy_in(&self, offset: u64, bytes: &[u8], wake: bool) -> Result<(), HandlerError> {
        let len = self.backend.chunk_size().min(bytes.len() as u64);
        self.install_spanning(offset, len, |host_va, seg_len, pos| {
            // SAFETY: `bytes[pos..pos+seg_len]` is in bounds
            // (`install_spanning` never advances past `len`, which is
            // clamped to `bytes.len()`). `host_va` is a guest-visible
            // address in a UFFD-registered region we hold copy access
            // to. UFFDIO_COPY installs the PTE + wakes per page atomically.
            unsafe {
                self.uffd.copy(
                    bytes[pos..pos + seg_len].as_ptr() as *const _,
                    host_va as *mut std::ffi::c_void,
                    seg_len,
                    wake,
                )?;
            }
            Ok(())
        })
    }

    /// Zero `len` bytes from `offset`: `UFFDIO_ZEROPAGE`, so the kernel
    /// maps its shared zero page and the guest's first write copies it.
    /// Some kernels reject ZEROPAGE on the substrate's MAP_PRIVATE file
    /// mapping; then this and every later zero install copies a zeroed
    /// buffer (one private page per touched page).
    fn zero_in(&self, offset: u64, len: u64, wake: bool) -> Result<(), HandlerError> {
        self.install_spanning(offset, len, |host_va, seg_len, _pos| {
            if self.zeropage_ok.load(Ordering::Relaxed) {
                // SAFETY: `host_va` is a guest-visible address in a UFFD-
                // registered region we hold zeropage access to. ZEROPAGE
                // installs zero pages over the range and wakes blocked
                // vCPUs atomically per page when `wake`.
                match unsafe {
                    self.uffd
                        .zeropage(host_va as *mut std::ffi::c_void, seg_len, wake)
                } {
                    Ok(_) => return Ok(()),
                    Err(userfaultfd::Error::ZeropageFailed(errno))
                        if errno as i32 == libc::EINVAL || errno as i32 == libc::EOPNOTSUPP =>
                    {
                        self.zeropage_ok.store(false, Ordering::Relaxed);
                        tracing::info!(
                            "UFFDIO_ZEROPAGE unsupported on this mapping; installing copies of zeros"
                        );
                    }
                    Err(e) => return Err(HandlerError::Uffd(e)),
                }
            }
            let zeros = vec![0u8; seg_len];
            // SAFETY: a zeroed buffer of `seg_len`; same contract as
            // `copy_in`'s copy.
            unsafe {
                self.uffd.copy(
                    zeros.as_ptr() as *const _,
                    host_va as *mut std::ffi::c_void,
                    seg_len,
                    wake,
                )?;
            }
            Ok(())
        })
    }

    /// ADR 0045 substrate (v2b): map a canonical chunk from the base shm
    /// file with `UFFDIO_CONTINUE` — the kernel maps the base file's
    /// page-cache pages (write-protected MAP_PRIVATE; guest writes COW),
    /// so every same-template VM on the host shares one physical copy.
    /// `bytes` are written to the base first when it does not hold them;
    /// they are `None` only when it does.
    fn continue_from_base(
        &self,
        base: &crate::base_shm::BaseShm,
        offset: u64,
        len: u64,
        bytes: Option<Bytes>,
        wake: bool,
    ) -> Result<(), HandlerError> {
        if let Some(bytes) = bytes {
            if !base.is_populated(offset, len) {
                base.write_chunk(offset, &bytes)
                    .map_err(HandlerError::BaseShm)?;
            }
        }
        // CONTINUE each region segment the chunk spans. Per segment the
        // kernel may map a prefix and return progress (looped) and may
        // EEXIST on a page a racing fault already mapped.
        self.install_spanning(offset, len, |host_va, seg_len, _pos| {
            let mut done: u64 = 0;
            while (done as usize) < seg_len {
                let start = host_va + done;
                let len = seg_len as u64 - done;
                // (`Uffd::continue` is a safe wrapper — the range lies inside a
                // UFFD-registered region whose backing pages we just ensured
                // are present in the base file's page cache.)
                match self
                    .uffd
                    .r#continue(start as *mut std::ffi::c_void, len as usize, wake)
                {
                    Ok(0) => break, // defensive: no progress
                    Ok(mapped) => done += mapped,
                    Err(userfaultfd::Error::SystemError(e)) if e as i32 == libc::EEXIST => {
                        // Page(s) already mapped (racing fault). The kernel
                        // stops at the first conflict without reporting
                        // progress; skip one page and keep going.
                        done += self.page_size;
                    }
                    Err(e) => return Err(HandlerError::Uffd(e)),
                }
            }
            Ok(())
        })
    }

    /// Run forever, draining events from the UFFD and serving each
    /// page fault. Returns `Ok(())` cleanly when the UFFD is closed
    /// (Firecracker exited / sandbox destroyed).
    ///
    /// [`crate::FAULT_THREADS`] threads read the userfaultfd. The kernel
    /// gives each fault message to exactly one reader, and the claim table
    /// makes two installs of one chunk safe: one thread installs, the
    /// others wait for it and wake. The first thread to end (end of file
    /// or an error) ends the handler; the others end with the process.
    pub fn run(self: &Arc<Self>) -> Result<(), HandlerError> {
        let threads = crate::FAULT_THREADS;
        let (done, ended) = std::sync::mpsc::channel();
        for i in 0..threads {
            let rt = Arc::clone(self);
            let done = done.clone();
            std::thread::Builder::new()
                .name(format!("engram-uffd-fault-{i}"))
                .spawn(move || {
                    let _ = done.send(rt.serve_faults());
                })?;
        }
        drop(done);
        tracing::info!(threads, "serving faults");
        ended.recv().unwrap_or(Ok(()))
    }

    fn serve_faults(self: &Arc<Self>) -> Result<(), HandlerError> {
        loop {
            match self.uffd.read_event() {
                Ok(Some(Event::Pagefault { addr, .. })) => {
                    let started = std::time::Instant::now();
                    fault_stats::take_fetch_source();
                    let arm = self.serve_pagefault_classified(addr as u64)?;
                    let source = match arm {
                        Source::Peer => Source::Peer,
                        arm => fault_stats::take_fetch_source().unwrap_or(arm),
                    };
                    let served = fault_stats::STATS.record_fault(source, started.elapsed());
                    if served.is_power_of_two() {
                        fault_stats::STATS.log("pow2");
                    }
                }
                Ok(Some(other)) => {
                    tracing::debug!(?other, "non-pagefault uffd event");
                }
                Ok(None) => {
                    return Ok(());
                }
                Err(userfaultfd::Error::ReadEof) => {
                    tracing::info!("uffd closed by firecracker; handler exiting cleanly");
                    return Ok(());
                }
                Err(e) => return Err(HandlerError::Uffd(e)),
            }
        }
    }

    /// The fill for a verified peer response, its bytes fetched now: the
    /// sealed output records them, and the fault window must not fetch
    /// under its cancellation lock. `AltSource` (live content equals the
    /// durable chunk) takes the manifest's fill.
    fn peer_fill(
        backend: &ChunkedMemoryBackend,
        handle: &TokioHandle,
        offset: u64,
        page: crate::peer::PeerPage,
    ) -> Result<Fill, HandlerError> {
        use crate::peer::PeerPage;
        match page {
            PeerPage::Bytes(bytes) => Ok(Fill::Private(Payload::Bytes(Bytes::from(bytes)))),
            PeerPage::Zero => Ok(Fill::Zero),
            PeerPage::AltSource(_) => Self::fill_for(backend, offset)
                .ok_or_else(|| {
                    HandlerError::PeerLost(format!("AltSource for unresolvable offset {offset:#x}"))
                })?
                .fetched(backend, handle),
        }
    }

    /// Install one verified peer response without waking adjacent pages.
    fn install_peer_page(
        &self,
        offset: u64,
        page: crate::peer::PeerPage,
    ) -> Result<bool, HandlerError> {
        let fill = Self::peer_fill(&self.backend, &self.handle, offset, page)?;
        self.install(offset, fill, Installer::Peer)
    }

    #[cfg(test)]
    fn serve_pagefault(self: &Arc<Self>, fault_addr: u64) -> Result<(), HandlerError> {
        self.serve_pagefault_classified(fault_addr).map(|_| ())
    }

    /// Serve one fault and return where its bytes came from. A chunk
    /// fetch overrides the returned source through
    /// [`fault_stats::set_fetch_source`].
    fn serve_pagefault_classified(
        self: &Arc<Self>,
        fault_addr: u64,
    ) -> Result<Source, HandlerError> {
        // Locate the region this fault came from to translate
        // host_va → byte_offset (the inverse of `locate_offset`).
        let region = self
            .mappings
            .iter()
            .find(|r| r.contains(fault_addr))
            .ok_or(HandlerError::AddressOutsideRegions(fault_addr))?;
        let page_size = region.page_size as u64;
        let page_aligned = fault_addr & !(page_size - 1);
        let intra = page_aligned - region.base_host_virt_addr;
        let byte_offset = region.offset + intra;

        // Round down to chunk-aligned so the full-chunk install
        // lands consistently across faults in the same chunk.
        let chunk_size = self.backend.chunk_size();
        let chunk_byte_offset = byte_offset - (byte_offset % chunk_size);
        let chunk_idx = self.chunk_idx(chunk_byte_offset);
        self.first_touch.touch(chunk_idx);

        // ADR 0045 C2: sealed chunks are peer-authoritative — their truth
        // lives only in the paused source's address space, so they MUST
        // resolve via the peer, never via `resolve()` (whose manifest view
        // predates the dirtying). Ahead of everything else by design.
        if let Some(peer) = self.peer.as_ref() {
            if peer.seal().get(chunk_idx as u64) {
                if self.chunk_installed(chunk_idx) {
                    // Drain (or an earlier fault) already landed it; the
                    // vCPU queued before the install — wake it.
                    self.wake_page(page_aligned, page_size)?;
                    return Ok(Source::Wake);
                }
                // ADR 0045 C2 / issue #227 (a): once the drain has pulled
                // every sealed chunk, the source's export may be torn down
                // (migration_commit) and dialing it would park up to 120 s
                // then fatally escalate a HEALTHY, fully-drained dest. After
                // DrainDone the chunk is installed (handled above) or — only
                // under a genuine drain bug — still missing, in which case we
                // fall through to resolve() rather than rewinding the VM.
                if self.is_drain_done() {
                    tracing::warn!(
                        chunk_idx,
                        "sealed fault after DrainDone with chunk uninstalled; \
                         not dialing torn-down peer, falling through to resolve()"
                    );
                } else {
                    let offsets = crate::peer::fault_window(
                        peer.seal(),
                        chunk_byte_offset,
                        region.offset + region.size as u64,
                        self.fault_around_chunks,
                        |idx| self.chunk_installed(idx),
                    );
                    if offsets.is_empty() {
                        self.wake_page(page_aligned, page_size)?;
                        return Ok(Source::Wake);
                    }
                    match self.serve_fault_window(peer, offsets, chunk_byte_offset) {
                        Ok(()) => {
                            self.wake_page(page_aligned, page_size)?;
                            return Ok(Source::Peer);
                        }
                        Err(e) => {
                            if self.chunk_installed(chunk_idx) {
                                self.wake_page(page_aligned, page_size)?;
                                return Ok(Source::Wake);
                            }
                            self.report_window_failure(peer, &e.to_string());
                            return Err(e);
                        }
                    }
                }
            }
        }

        let fill = Self::fill_for(&self.backend, chunk_byte_offset)
            .ok_or(HandlerError::AddressOutsideRegions(fault_addr))?;
        let zero = matches!(fill, Fill::Zero);
        if !self.install(chunk_byte_offset, fill, Installer::Fault)? {
            // Another installer had it: the kernel may have queued this
            // fault before that install landed, so wake the vCPU.
            self.wake_page(page_aligned, page_size)?;
            return Ok(Source::Wake);
        }
        Ok(if zero { Source::Zero } else { Source::Shm })
    }

    /// Cheap read of whether a chunk is fully `Installed` (a chunk that
    /// is merely `Installing` is NOT yet serveable). Drain/seal
    /// accounting relies on this being `Installed`-exact so
    /// `sealed_uninstalled_count` doesn't under-report a chunk whose
    /// install failed and rolled back (issue #206).
    fn chunk_installed(&self, chunk_idx: usize) -> bool {
        self.installed
            .lock()
            .expect("installed bitmap poisoned")
            .get(chunk_idx)
            .copied()
            .map(|s| s == ChunkState::Installed)
            .unwrap_or(false)
    }

    fn report_window_failure(&self, peer: &crate::peer::PeerSession, detail: &str) {
        peer.mark_lost();
        if let Some(control) = &self.control {
            control.report(engram_migrate_proto::HandlerControl::PeerLost {
                remaining: self.sealed_uninstalled_count(peer),
                detail: detail.to_owned(),
            });
        }
    }

    fn install_window_page(
        &self,
        offset: u64,
        page: crate::peer::PeerPage,
    ) -> Result<(), HandlerError> {
        if self.install_peer_page(offset, page)? {
            self.fault_around_chunks_installed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }

    /// Only the requested chunk holds fault priority. A chunk already on
    /// the wire (drain or window) is awaited, not asked for again. A busy
    /// prefetch slot drops new speculative work; every other demand uses
    /// the fault connection.
    fn serve_fault_window(
        self: &Arc<Self>,
        peer: &Arc<crate::peer::PeerSession>,
        offsets: Vec<u64>,
        fault_offset: u64,
    ) -> Result<(), HandlerError> {
        use crate::peer::PEER_READ_TIMEOUT;
        let chunk_size = self.backend.chunk_size();
        let fault_idx = (fault_offset / chunk_size) as usize;
        let mut slot = self.fault_window.lock().expect("window slot poisoned");
        if slot.as_ref().is_some_and(|job| job.thread.is_finished()) {
            slot.take().unwrap().stop();
        }
        if self.is_drain_done() || self.chunk_installed(fault_idx) {
            return Ok(());
        }
        if self.is_requested(fault_idx) {
            drop(slot);
            let _fault = peer.begin_fault();
            if self.await_requested(fault_idx) {
                return Ok(());
            }
            slot = self.fault_window.lock().expect("window slot poisoned");
        }
        if slot.is_some() {
            drop(slot);
            // Overlapping demands also bypass the worker. Install claims
            // deduplicate its later response without waiting for the suffix.
            let owned = self.try_request(fault_idx);
            let page = peer.need_at(fault_offset);
            let installed = page
                .map_err(|e| HandlerError::PeerLost(e.to_string()))
                .and_then(|page| self.install_window_page(fault_offset, page));
            if owned {
                self.clear_request(fault_idx);
            }
            return installed;
        }
        if peer.is_lost() {
            return Err(HandlerError::PeerLost("peer already marked lost".into()));
        }
        let _fault = peer.begin_fault();
        // The window asks only for chunks no other request holds; the
        // faulting chunk is always asked for.
        let mut owned = Vec::with_capacity(offsets.len());
        let offsets: Vec<u64> = offsets
            .into_iter()
            .filter(|offset| {
                let idx = (offset / chunk_size) as usize;
                let mine = self.try_request(idx);
                if mine {
                    owned.push(idx);
                }
                mine || *offset == fault_offset
            })
            .collect();
        let progress = Arc::new(WindowProgress::new());
        let worker_progress = progress.clone();
        let runtime = Arc::downgrade(self);
        let backend = self.backend.clone();
        let handle = self.handle.clone();
        let worker_peer = peer.clone();
        let (first_tx, first_rx) = std::sync::mpsc::sync_channel(1);
        let (installed_tx, installed_rx) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("uffd-fault-around".into())
            .spawn(move || {
                let connected = |conn: Option<&std::net::TcpStream>| {
                    let mut socket = worker_progress.socket.lock().expect("window poisoned");
                    if worker_progress.stopped() {
                        return Ok(false);
                    }
                    if let Some(conn) = conn {
                        *socket = Some(conn.try_clone()?);
                    }
                    Ok(true)
                };
                let result = (|| -> Result<(), String> {
                    let mut conn = worker_peer
                        .open_prefetch_conn(connected)
                        .map_err(|e| e.to_string())?;
                    worker_peer
                        .stream_window(
                            &mut conn,
                            &offsets,
                            |offset, page| {
                                if worker_progress.stopped() {
                                    return Ok(());
                                }
                                if offset == fault_offset {
                                    first_tx.send(page).map_err(|e| e.to_string())?;
                                    installed_rx
                                        .recv_timeout(PEER_READ_TIMEOUT)
                                        .map_err(|e| e.to_string())?;
                                } else {
                                    // Fetches may outlive stop(). They own only the backend,
                                    // never the runtime or its mappings.
                                    let fill = Self::peer_fill(&backend, &handle, offset, page)
                                        .map_err(|e| e.to_string())?;
                                    if worker_progress.stopped() {
                                        return Ok(());
                                    }
                                    let Some(rt) = runtime.upgrade() else {
                                        return Ok(());
                                    };
                                    let chunk_idx = (offset / backend.chunk_size()) as usize;
                                    // A competing installer may be fetching durable bytes too.
                                    // Wait for its claim outside the cancellation lock.
                                    if rt.claim_for_install(chunk_idx) != Claim::Claimed {
                                        return Ok(());
                                    }
                                    let _guard =
                                        worker_progress.socket.lock().expect("window poisoned");
                                    if worker_progress.stopped() {
                                        rt.release_chunk(chunk_idx);
                                        return Ok(());
                                    }
                                    if rt
                                        .install_claimed(offset, fill, Installer::Peer)
                                        .map_err(|e| e.to_string())?
                                    {
                                        rt.fault_around_chunks_installed
                                            .fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                Ok::<_, String>(())
                            },
                            || worker_progress.stopped(),
                            connected,
                        )
                        .map_err(|e| e.to_string())??;
                    // Every response was read: the connection is idle and
                    // can serve the next window. Unregister it first, so a
                    // later stop() of this window cannot shut it down.
                    let mut socket = worker_progress.socket.lock().expect("window poisoned");
                    if !worker_progress.stopped() {
                        *socket = None;
                        drop(socket);
                        worker_peer.return_prefetch_conn(conn);
                    }
                    Ok(())
                })();
                if let Some(rt) = runtime.upgrade() {
                    for idx in owned {
                        rt.clear_request(idx);
                    }
                }
                if let Err(error) = result {
                    if let Some(rt) = runtime.upgrade() {
                        let message = engram_migrate_proto::HandlerControl::PeerLost {
                            remaining: rt.sealed_uninstalled_count(&worker_peer),
                            detail: error,
                        };
                        let publish = || {
                            let _guard = worker_progress.socket.lock().expect("window poisoned");
                            if worker_progress.stopped() {
                                return false;
                            }
                            if rt.sealed_uninstalled_count(&worker_peer) == 0 {
                                tracing::debug!(
                                    "discarding fault-window failure: all sealed chunks installed"
                                );
                                return false;
                            }
                            worker_peer.mark_lost();
                            true
                        };
                        if let Some(control) = &rt.control {
                            control.report_if(message, publish);
                        } else {
                            publish();
                        }
                    }
                }
            })?;
        *slot = Some(FaultWindow { progress, thread });
        drop(slot);
        let page = first_rx
            .recv_timeout(crate::peer::WINDOW_FIRST_RESPONSE_TIMEOUT)
            .map_err(|e| HandlerError::PeerLost(format!("fault window first response: {e}")))?;
        self.install_window_page(fault_offset, page)?;
        // The fault loop installs the first response. The worker owns all later
        // responses and never wakes a page or holds the fault-active guard.
        let _ = installed_tx.send(());
        Ok(())
    }

    fn stop_fault_window(&self) {
        let mut slot = self.fault_window.lock().expect("window slot poisoned");
        if let Some(job) = slot.take() {
            job.stop();
        }
        // Keep the slot locked until new windows are prohibited.
        self.drain_done
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Cancel the prefetch worker and bound the join before publishing DrainDone.
    fn mark_drain_done(&self) {
        self.stop_fault_window();
    }

    #[cfg(test)]
    fn finish_fault_window(&self) {
        if let Some(job) = self.fault_window.lock().unwrap().take() {
            job.thread.join().unwrap();
        }
    }

    /// True once the background drain has reported DrainDone.
    fn is_drain_done(&self) -> bool {
        self.drain_done.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// How many sealed chunks remain uninstalled (PeerLost reporting).
    fn sealed_uninstalled_count(&self, peer: &crate::peer::PeerSession) -> u64 {
        let installed = self.installed.lock().expect("installed bitmap poisoned");
        (0..peer.seal().chunk_count)
            .filter(|i| {
                peer.seal().get(*i)
                    && installed.get(*i as usize).copied() != Some(ChunkState::Installed)
            })
            .count() as u64
    }

    /// ADR 0045 C2: the background drain — pull every sealed chunk the
    /// fault path hasn't already served, over the drain's OWN
    /// connections, pipelined. Runs FIRST on the producer thread (before
    /// the eager sweep): sealed content exists only in the paused source,
    /// so draining it is what releases the source — and it makes the
    /// later sweep trivially safe (by the time it runs, every sealed chunk
    /// is installed, so its stale `resolve()` view can never be installed
    /// over divergence).
    ///
    /// A large drain runs on [`DRAIN_CONNS`] connections; the source
    /// serves each connection on its own thread. A chunk that a fault
    /// already requested is skipped until that request ends; it is asked
    /// for again only if it is still not installed then.
    ///
    /// `hot` (the source's hot chunks, hottest first) leads the drain.
    /// `AltSource` responses demote inline to the class-2 path (fetch
    /// via cache/store + private install). Install errors are real
    /// errors; peer errors return `Err` and the caller reports
    /// `PeerLost`.
    pub fn drain_from_peer(
        &self,
        peer: &crate::peer::PeerSession,
        hot: &[ChunkHash],
    ) -> Result<crate::peer::DrainStats, HandlerError> {
        self.drain_with_hint(peer, hot, &mut || None)
    }

    /// [`Self::drain_from_peer`], with `hint` polled once per response:
    /// the first hot set it returns reorders the chunks still queued.
    fn drain_with_hint(
        &self,
        peer: &crate::peer::PeerSession,
        hot: &[ChunkHash],
        hint: &mut impl FnMut() -> Option<Vec<ChunkHash>>,
    ) -> Result<crate::peer::DrainStats, HandlerError> {
        use crate::peer::DrainStats;

        let chunk_size = self.backend.chunk_size();
        let seal = peer.seal();
        let mut stats = DrainStats::default();
        let mut conn = peer
            .open_extra_conn()
            .map_err(|e| HandlerError::PeerLost(format!("drain conn: {e}")))?;

        // Pending sealed chunk offsets, skipping anything already
        // installed by a racing fault. HOT-FIRST: the source's
        // resume-time working set (in first-fault order) leads, so
        // the chunks the guest touches first are installed first and
        // most would-be faults are already local when they happen —
        // the no-added-pause version of a hot-set pre-install. The
        // cold remainder keeps offset order.
        let mut todo: Vec<u64> = Vec::with_capacity(seal.count_ones() as usize);
        let mut queued = std::collections::HashSet::new();
        for hash in hot {
            for offset in self.backend.session_positions_of(*hash) {
                let idx = offset / chunk_size;
                if seal.get(idx) && queued.insert(idx) {
                    todo.push(idx * chunk_size);
                }
            }
        }
        let hot_leading = todo.len();
        stats.hot_leading = hot_leading as u64;
        stats.hot_hint_received = !hot.is_empty();
        for i in (0..seal.chunk_count).filter(|i| seal.get(*i)) {
            if !queued.contains(&i) {
                todo.push(i * chunk_size);
            }
        }
        if hot_leading > 0 {
            tracing::info!(hot_leading, total = todo.len(), "drain ordered hot-first");
        }
        let extra_conns = if todo.len() > DRAIN_SPLIT_MIN_CHUNKS {
            DRAIN_CONNS - 1
        } else {
            0
        };
        let shared = DrainShared::new(todo);

        let mut apply_hint = || {
            let Some(hot) = hint() else { return };
            let mut todo = shared.todo.lock().expect("drain queue poisoned");
            stats.hot_hint_received |= !hot.is_empty() && !todo.is_empty();
            let mut rank = std::collections::HashMap::new();
            for hash in &hot {
                for offset in self.backend.session_positions_of(*hash) {
                    let next = rank.len();
                    rank.entry(offset).or_insert(next);
                }
            }
            stats.hot_leading += todo
                .iter()
                .filter(|(o, _)| {
                    rank.contains_key(o) && !self.chunk_installed((*o / chunk_size) as usize)
                })
                .count() as u64;
            todo.make_contiguous()
                .sort_by_key(|(o, _)| rank.get(o).copied().unwrap_or(usize::MAX));
        };
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..extra_conns)
                .map(|_| {
                    scope.spawn(|| -> Result<(), HandlerError> {
                        // An extra connection only adds throughput: the
                        // first connection drains alone without it.
                        let mut conn = match peer.open_extra_conn() {
                            Ok(conn) => conn,
                            Err(error) => {
                                tracing::warn!(%error, "extra drain connection failed");
                                return Ok(());
                            }
                        };
                        self.drain_conn(peer, &shared, &mut conn, &mut || {})
                    })
                })
                .collect();
            let mut result = self.drain_conn(peer, &shared, &mut conn, &mut apply_hint);
            for worker in workers {
                let joined = worker.join().unwrap_or_else(|_| {
                    Err(HandlerError::PeerLost("drain worker panicked".into()))
                });
                if result.is_ok() {
                    result = joined;
                }
            }
            result
        })?;

        stats.pulled = shared.pulled.load(Ordering::Relaxed);
        stats.alt_sourced = shared.alt_sourced.load(Ordering::Relaxed);
        stats.zero_chunks = shared.zero_chunks.load(Ordering::Relaxed);
        let _ = crate::peer::send_drain_done(&mut conn, stats);
        Ok(stats)
    }

    /// One drain connection: pipeline requests from the shared queue and
    /// install the responses, until the queue and the deferred set are
    /// empty or another connection failed. `tick` runs once per response.
    fn drain_conn(
        &self,
        peer: &crate::peer::PeerSession,
        shared: &DrainShared,
        conn: &mut std::net::TcpStream,
        tick: &mut dyn FnMut(),
    ) -> Result<(), HandlerError> {
        let result = self.drain_conn_inner(peer, shared, conn, tick);
        if result.is_err() {
            shared.failed.store(true, Ordering::SeqCst);
        }
        result
    }

    fn drain_conn_inner(
        &self,
        peer: &crate::peer::PeerSession,
        shared: &DrainShared,
        conn: &mut std::net::TcpStream,
        tick: &mut dyn FnMut(),
    ) -> Result<(), HandlerError> {
        use crate::peer::PeerPage;
        // Per-request (retryable, `Error{req_id: Some}`) drain failures get
        // a bounded same-conn retry before the drain gives up (issue #227 b).
        const DRAIN_REQUEST_RETRIES: u32 = 3;
        const DRAIN_REQUEST_BACKOFF: Duration = Duration::from_millis(50);

        let chunk_size = self.backend.chunk_size();
        let mut in_flight: std::collections::VecDeque<(u64, u64)> = Default::default(); // (req_id, offset)
                                                                                        // Issue #227 (b): offsets the source answered with a per-request
                                                                                        // error (`Error{req_id: Some}`, e.g. a transient process_vm_readv
                                                                                        // EAGAIN/ENOMEM). Re-queued for a bounded re-request through the
                                                                                        // SAME pipeline (preserving strict request/response FIFO order on
                                                                                        // the wire — never a side-band read) rather than rewinding the
                                                                                        // whole drain. `attempts[offset]` bounds the retries. Their
                                                                                        // request flags stay set until the chunk is installed.
        let mut retry: std::collections::VecDeque<u64> = Default::default();
        let mut attempts: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
        let abandon = |in_flight: &std::collections::VecDeque<(u64, u64)>,
                       retry: &std::collections::VecDeque<u64>| {
            for offset in in_flight.iter().map(|(_, o)| o).chain(retry.iter()) {
                self.clear_request((offset / chunk_size) as usize);
            }
        };
        loop {
            if shared.failed.load(Ordering::SeqCst) {
                abandon(&in_flight, &retry);
                return Ok(());
            }
            tick();
            // Yield most of the wire to guest faults: a fault request
            // that queues behind bulk drain bytes stalls a vCPU, while
            // the drain's finish time only delays the source release.
            let depth = if peer.fault_active_within(FAULT_RECENT) {
                FAULT_SHARE_DEPTH
            } else {
                PIPELINE_DEPTH
            };
            while in_flight.len() < depth {
                let (offset, forced) = match retry.pop_front() {
                    Some(o) => (o, true),
                    None => match shared.pop() {
                        Some(next) => next,
                        None => break,
                    },
                };
                let idx = (offset / chunk_size) as usize;
                if self.chunk_installed(idx) {
                    shared.resolved(self);
                    continue;
                }
                if !self.try_request(idx) && !forced {
                    shared.defer(offset);
                    continue;
                }
                let req_id = shared.next_req.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = engram_migrate_proto::write_frame(
                    &mut *conn,
                    &engram_migrate_proto::ToSource::NeedAt {
                        req_id,
                        chunk_offset: offset,
                    },
                ) {
                    self.clear_request(idx);
                    abandon(&in_flight, &retry);
                    return Err(HandlerError::PeerLost(format!("drain write: {e}")));
                }
                in_flight.push_back((req_id, offset));
            }
            let Some((req_id, offset)) = in_flight.pop_front() else {
                if retry.is_empty() && !self.settle_deferred(shared) {
                    return Ok(());
                }
                continue;
            };
            let idx = (offset / chunk_size) as usize;
            let resp = match engram_migrate_proto::read_frame(&mut *conn) {
                Ok(resp) => resp,
                Err(e) => {
                    self.clear_request(idx);
                    abandon(&in_flight, &retry);
                    return Err(HandlerError::PeerLost(format!("drain read: {e}")));
                }
            };
            // Issue #227 (b): a per-request server error (`Error{req_id:
            // Some}`) is NOT migration-fatal — the conn keeps serving. Bound-
            // retry THAT chunk via the re-queue (FIFO-safe) instead of
            // rewinding. Connection-fatal errors (`Error{None}`/IO/sha/
            // geometry) still escalate to PeerLost.
            let page = match crate::peer::decode_page(resp, req_id, offset) {
                Ok(page) => page,
                Err(crate::peer::PeerError::RequestFailed(msg)) => {
                    let n = attempts.entry(offset).or_insert(0);
                    *n += 1;
                    if *n > DRAIN_REQUEST_RETRIES {
                        self.clear_request(idx);
                        abandon(&in_flight, &retry);
                        return Err(HandlerError::PeerLost(format!(
                            "drain per-request retries exhausted at offset {offset:#x}: {msg}"
                        )));
                    }
                    tracing::warn!(
                        offset,
                        attempt = *n,
                        error = %msg,
                        "drain per-request error; re-queuing for retry"
                    );
                    std::thread::sleep(DRAIN_REQUEST_BACKOFF);
                    retry.push_back(offset);
                    continue;
                }
                Err(e) => {
                    self.clear_request(idx);
                    abandon(&in_flight, &retry);
                    return Err(HandlerError::PeerLost(format!("drain decode: {e}")));
                }
            };
            let counter = match &page {
                PeerPage::Bytes(_) => &shared.pulled,
                PeerPage::Zero => &shared.zero_chunks,
                PeerPage::AltSource(_) => &shared.alt_sourced,
            };
            let installed = self.install_peer_page(offset, page);
            self.clear_request(idx);
            if let Err(e) = installed {
                abandon(&in_flight, &retry);
                return Err(e);
            }
            counter.fetch_add(1, Ordering::Relaxed);
            shared.resolved(self);
        }
    }

    /// Wait for the chunks other requests held, and queue the ones still
    /// not installed for a request of their own. `false` when nothing
    /// was deferred.
    fn settle_deferred(&self, shared: &DrainShared) -> bool {
        let deferred = std::mem::take(&mut *shared.deferred.lock().expect("drain queue poisoned"));
        if deferred.is_empty() {
            return false;
        }
        let chunk_size = self.backend.chunk_size();
        for offset in deferred {
            let idx = (offset / chunk_size) as usize;
            if self.chunk_installed(idx) || self.await_requested(idx) {
                shared.resolved(self);
            } else {
                shared.force(offset);
            }
        }
        true
    }

    /// `UFFDIO_WAKE` the specified page. Used when the chunk this
    /// fault belongs to was already installed (by the sweep, the
    /// drain, or a sibling fault): the page is no longer
    /// missing but the vCPU was still queued, so we have to tell
    /// the kernel explicitly.
    fn wake_page(&self, page_aligned: u64, page_size: u64) -> Result<(), HandlerError> {
        self.uffd
            .wake(page_aligned as *mut std::ffi::c_void, page_size as usize)?;
        Ok(())
    }

    /// Test/debug accessor.
    #[allow(dead_code)]
    pub fn mappings(&self) -> &[GuestRegionUffdMapping] {
        &self.mappings
    }
}

/// One-shot run: bind the listener, accept Firecracker, handshake,
/// then serve faults until the UFFD closes.
///
/// `stream` is held alive for the lifetime of the run — matches the
/// upstream `on_demand_handler.rs` which polls both the stream and
/// the UFFD. Dropping the stream after the handshake caused
/// Firecracker to hang on `PUT /snapshot/load` (it keeps its side
/// open and treats our close as a protocol error).
pub struct RunListenerOpts {
    pub fault_around_chunks: usize,
    /// Peer mode: the advisory hot-set hint, read concurrently with the
    /// drain (see [`crate::hot_hint`]).
    pub migration_hint: Option<PathBuf>,
    /// Where the first-touch record and the drain stats go.
    pub report_dir: Option<PathBuf>,
    pub base_shm: Option<crate::base_shm::BaseShm>,
    /// ADR 0045 C2: post-copy peer mode (session already sealed) +
    /// the optional control-sock reporter.
    pub peer: Option<(
        std::sync::Arc<crate::peer::PeerSession>,
        Option<std::sync::Arc<crate::peer::ControlTx>>,
    )>,
    /// Peer mode: record every peer-supplied chunk here (see
    /// [`crate::peer::SealedOutput`]).
    pub sealed_output: Option<PathBuf>,
    /// The session's published first-touch prefix, as it loads; the
    /// first-touch recorder carries it forward.
    pub previous_prefix: Option<tokio::task::JoinHandle<Option<Vec<u32>>>>,
}

pub fn run_listener(
    listen: PathBuf,
    backend: Arc<ChunkedMemoryBackend>,
    handle: TokioHandle,
    opts: RunListenerOpts,
) -> Result<(), HandlerError> {
    let RunListenerOpts {
        fault_around_chunks,
        migration_hint,
        report_dir,
        base_shm,
        peer,
        sealed_output,
        previous_prefix,
    } = opts;
    let pid = std::process::id();
    let _ = std::fs::remove_file(&listen);
    let listener = std::os::unix::net::UnixListener::bind(&listen)?;
    tracing::info!(pid, socket = %listen.display(), "engram-uffd-handler listening");
    let (stream, _addr) = listener.accept()?;
    tracing::info!(pid, "Firecracker connected; awaiting handshake");
    let (mappings, uffd) = recv_handshake(&stream)?;
    let uffd_fd = AsRawFd::as_raw_fd(&uffd);
    make_blocking(uffd_fd)?;
    let total: usize = mappings.iter().map(|m| m.size).sum();
    tracing::info!(
        pid,
        uffd_fd,
        regions = mappings.len(),
        total_bytes = total,
        "handshake complete",
    );
    let mut rt = Runtime::new(mappings, uffd, backend, handle, base_shm)?;
    rt.fault_around_chunks = fault_around_chunks;
    if let Some(dir) = report_dir {
        rt.set_report_dir(dir);
    }
    if let Some((session, control)) = peer {
        rt.set_peer(session, control);
        if let Some(path) = sealed_output {
            match crate::peer::SealedOutput::create(path) {
                Ok(out) => rt.set_sealed_output(out),
                Err(error) => {
                    tracing::warn!(%error, "sealed output unavailable; the first checkpoint is Full")
                }
            }
        }
    }
    // ADR 0043 P1: the drain and the sweep run on a BACKGROUND producer
    // concurrent with the fault loop; vCPUs run the instant Firecracker
    // resumes. The claim table makes each install exactly-once, and a fault
    // that finds its chunk installed by the producer wakes its vCPU.
    let rt = Arc::new(rt);
    if let Some(prefix) = previous_prefix {
        let rt = Arc::clone(&rt);
        rt.handle.clone().spawn(async move {
            if let Ok(Some(indices)) = prefix.await {
                rt.first_touch.carry(&indices);
            }
        });
    }
    {
        // Off the fault path: the composition stats every divergent chunk.
        let composition = Arc::clone(&rt);
        let _ = std::thread::Builder::new()
            .name("engram-uffd-composition".to_string())
            .spawn(move || composition.log_composition());
        fault_stats::spawn_periodic_log();
    }
    let producer = {
        let rt = Arc::clone(&rt);
        std::thread::Builder::new()
            .name("engram-uffd-producer".to_string())
            .spawn(move || {
                let (hint_tx, mut hint_rx) = tokio::sync::oneshot::channel();
                let hint_task = migration_hint.map(|path| rt.handle.spawn(async move {
                    match crate::hot_hint::load(&path).await {
                        Ok(hot) => { let _ = hint_tx.send(hot); }
                        Err(error) => tracing::info!(%error, "migration hint unavailable; drain continues"),
                    }
                }));
                // ADR 0045 C2: in peer mode the sealed drain runs FIRST —
                // sealed content exists only in the paused source, so
                // draining it is what releases the source, and it makes
                // the sweep below trivially safe (every sealed chunk is
                // installed before its stale resolve() view could touch it). A drain failure is PeerLost:
                // report and stop producing — the host-agent rewinds the
                // whole VM; warming a doomed guest is wasted I/O.
                if let Some(peer) = rt.peer.clone() {
                    let started = std::time::Instant::now();
                    let outcome = rt.drain_with_hint(&peer, &[], &mut || hint_rx.try_recv().ok());
                    if let Some(task) = hint_task { task.abort(); }
                    match outcome {
                        Ok(stats) => {
                            // Issue #227 (a): latch drain-complete BEFORE
                            // reporting DrainDone. The host-agent acts on
                            // DrainDone by committing the migration (which
                            // tears the source export down), so the fault
                            // path must already know not to dial the peer by
                            // the time that frame is observed.
                            rt.mark_drain_done();
                            // Every sealed chunk is installed, so the
                            // sealed output is complete.
                            if let Some(out) = rt.sealed_out.as_ref() {
                                out.finish();
                            }
                            let fault_around_chunks_installed = rt
                                .fault_around_chunks_installed
                                .load(std::sync::atomic::Ordering::Relaxed);
                            let (faults, fault_us, fault_max_us) = peer.fault_stats();
                            if let Some(dir) = rt.report_dir.as_ref() {
                                write_drain_stats(
                                    &dir.join(PREFAULT_STATS_FILE),
                                    &PeerDrainStats {
                                        peer_pulled: stats.pulled,
                                        peer_alt_sourced: stats.alt_sourced,
                                        peer_zero_chunks: stats.zero_chunks,
                                        peer_live_faults: faults,
                                        fault_around_chunks_installed,
                                        hot_leading: stats.hot_leading,
                                        hot_hint_received: stats.hot_hint_received,
                                    },
                                );
                            }
                            tracing::info!(
                                pulled = stats.pulled,
                                alt_sourced = stats.alt_sourced,
                                zero_chunks = stats.zero_chunks,
                                ms = started.elapsed().as_millis() as u64,
                                faults,
                                fault_us,
                                fault_max_us,
                                "post-copy drain complete"
                            );
                            fault_stats::STATS.log("drain");
                            if let Some(control) = rt.control.as_ref() {
                                control.report(engram_migrate_proto::HandlerControl::DrainDone {
                                    pulled: stats.pulled,
                                    alt_sourced: stats.alt_sourced,
                                    zero_chunks: stats.zero_chunks,
                                    ms: started.elapsed().as_millis() as u64,
                                    faults,
                                    fault_us,
                                    fault_max_us,
                                    fault_around_chunks_installed,
                                });
                            }
                        }
                        Err(e) => {
                            peer.mark_lost();
                            tracing::error!(error = %e, "post-copy drain failed (PeerLost)");
                            if let Some(control) = rt.control.as_ref() {
                                control.report(engram_migrate_proto::HandlerControl::PeerLost {
                                    remaining: rt.sealed_uninstalled_count(&peer),
                                    detail: e.to_string(),
                                });
                            }
                            return;
                        }
                    }
                }
                // The eager sweep installs what the cache holds, so by
                // the time the guest touches a page, the odds it must
                // round-trip the fault loop shrink to the race window.
                if let Err(e) = rt.sweep_all() {
                    tracing::warn!(
                        error = %e,
                        "eager full sweep failed; remaining pages serve on-demand",
                    );
                }
            })
    };
    let _stream_alive = stream;
    tracing::info!(pid, "starting fault loop");
    let result = rt.run();
    tracing::info!(pid, ?result, "fault loop returned");
    fault_stats::STATS.log("exit");
    rt.stop_fault_window();
    // Once the uffd closes (FC exited) the producer's next install errors
    // out, so this join returns promptly rather than blocking teardown.
    if let Ok(handle) = producer {
        let _ = handle.join();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunked::ChunkedMemoryBackend;
    use engram_chunk_store::cache::ChunkCacheConfig;
    use engram_chunk_store::manifest::{
        ChunkRef, ChunkSize, ManifestKind, MANIFEST_SCHEMA_VERSION,
    };
    use engram_chunk_store::{ChunkStore, Manifest};
    use engram_core::traits::BlobStorage;
    use engram_storage_local::LocalBlobStorage;
    use userfaultfd::UffdBuilder;

    /// The live-move tests read these field names from the stats file.
    #[test]
    fn drain_stats_file_keeps_its_field_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PREFAULT_STATS_FILE);
        write_drain_stats(
            &path,
            &PeerDrainStats {
                fault_around_chunks_installed: 3,
                hot_hint_received: true,
                ..Default::default()
            },
        );
        let stats: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(stats["fault_around_chunks_installed"], 3);
        assert_eq!(stats["hot_hint_received"], true);
        assert!(!dir.path().join("prefault-stats.json.tmp").exists());
    }

    /// ADR 0043 P1: the sweep runs on a background thread CONCURRENT with
    /// the fault loop. A sweep thread and a `serve_pagefault` thread, with an
    /// overlapping chunk range, must between them install every chunk
    /// EXACTLY once (the `installed` bitmap serialises the check-and-set)
    /// and leave the guest region byte-correct — no double-`UFFDIO_COPY`
    /// (`EEXIST`), no corruption, no panic. (The wake-a-blocked-vCPU half is
    /// `wake_page`, covered by the FC UFFD integration tests.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn concurrent_sweep_and_faults_install_every_chunk_once_and_correctly() {
        let page_size = 4096u64;
        let chunk_size = page_size; // one page per chunk keeps the math simple
        let n_chunks = 16usize;
        let total = chunk_size * n_chunks as u64;

        // Plant n_chunks of known content (chunk i = byte i+1 repeated) into a
        // local-backed store, and build a session==canonical manifest over them.
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = ChunkStore::new(blob);
        let mut entries = Vec::with_capacity(n_chunks);
        for i in 0..n_chunks {
            let tag = (i as u8).wrapping_add(1);
            let hash = store
                .put_chunk(&vec![tag; chunk_size as usize])
                .await
                .unwrap();
            entries.push(ChunkRef {
                offset: i as u64 * chunk_size,
                hash,
            });
        }
        let manifest = Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Memory,
            chunk_size: ChunkSize::bytes(chunk_size),
            total_bytes: total,
            chunks: entries,
            parent: None,
            annotations: serde_json::Value::Null,
        };
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let backend = Arc::new(
            ChunkedMemoryBackend::new(
                &manifest,
                &manifest,
                engram_chunk_store::reader::ChunkCacheReader::new(cfg.root.clone()),
                None,
                store,
            )
            .unwrap(),
        );

        // mmap a private region and create + register an (unprivileged,
        // user-mode-only) userfaultfd over it. Skip gracefully where the kernel
        // forbids unprivileged uffd (some CI sandboxes) rather than failing.
        let len = total as usize;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap failed");
        let uffd = match UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(false)
            .user_mode_only(true)
            .create()
        {
            Ok(u) => u,
            Err(e) => {
                unsafe { libc::munmap(ptr, len) };
                eprintln!(
                    "SKIP: cannot create userfaultfd ({e}); kernel forbids unprivileged uffd"
                );
                return;
            }
        };
        uffd.register(ptr, len).expect("register region with uffd");

        let mappings = vec![GuestRegionUffdMapping {
            base_host_virt_addr: ptr as u64,
            size: len,
            offset: 0,
            page_size: page_size as usize,
        }];
        let rt = Arc::new(
            Runtime::new(
                mappings,
                uffd,
                backend,
                tokio::runtime::Handle::current(),
                None,
            )
            .unwrap(),
        );

        // Two install paths race: the sweep covers every chunk; the "fault
        // server" thread serves chunks [n/4, n). Both call
        // `handle.block_on`, so they MUST run on plain threads (not tokio
        // workers) — exactly as the handler runs on a spawn_blocking thread.
        let base = ptr as u64;
        let rt_pf = Arc::clone(&rt);
        let prefault = std::thread::spawn(move || {
            rt_pf.sweep_all().expect("sweep");
        });
        let rt_sf = Arc::clone(&rt);
        let server = std::thread::spawn(move || {
            for i in (n_chunks / 4)..n_chunks {
                rt_sf
                    .serve_pagefault(base + i as u64 * chunk_size)
                    .expect("serve_pagefault");
            }
        });
        prefault.join().unwrap();
        server.join().unwrap();

        // Every chunk installed exactly once.
        let installed = rt.installed.lock().unwrap();
        assert!(
            installed.iter().all(|s| *s == ChunkState::Installed),
            "every chunk should be installed"
        );

        // Region is byte-correct — no page faults (all present), no corruption.
        let view = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        for i in 0..n_chunks {
            let tag = (i as u8).wrapping_add(1);
            let start = i * chunk_size as usize;
            assert!(
                view[start..start + chunk_size as usize]
                    .iter()
                    .all(|b| *b == tag),
                "chunk {i} content mismatch (expected {tag})",
            );
        }
        drop(installed);
        unsafe { libc::munmap(ptr, len) };
    }

    /// ADR 0045 C2: peer mode end-to-end at the Runtime level (real
    /// UFFD region, fake source server over loopback). Sealed chunks
    /// resolve from the PEER (Page bytes / ZeroChunk / AltSource
    /// demote) and the eager sweep covers the rest — with the sealed
    /// content (which differs from the manifest's stale view by
    /// construction) NEVER overwritten by sweep/resolve installs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn peer_mode_drains_sealed_chunks_and_sweep_never_overwrites_them() {
        for hint in [
            "unavailable",
            "empty",
            "delayed",
            "failed",
            "installed",
            "in-flight",
            "unsealed",
            "late-unsealed",
        ] {
            peer_drain_hint_case(hint).await;
        }
    }

    struct RecordingStorage {
        inner: LocalBlobStorage,
        fetched: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl BlobStorage for RecordingStorage {
        async fn put_streaming(
            &self,
            key: &str,
            body: engram_core::traits::ByteStream,
        ) -> Result<u64, engram_core::error::BlobError> {
            self.inner.put_streaming(key, body).await
        }

        async fn get_streaming(
            &self,
            key: &str,
        ) -> Result<engram_core::traits::ByteStream, engram_core::error::BlobError> {
            self.fetched.lock().unwrap().push(key.to_owned());
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

    async fn peer_drain_hint_case(hint_kind: &str) {
        use engram_migrate_proto::{
            read_frame, write_frame, FromSource, SealBitmap, ToSource, PROTO_VERSION,
        };
        let page_size = 4096u64;
        let chunk_size = page_size;
        let n_chunks = 20usize;
        let total = chunk_size * n_chunks as u64;
        const PEER_BYTE: u8 = 0xEE;

        // Manifest content: chunk i = byte i+1 (the STALE durable view).
        let dir = tempfile::tempdir().unwrap();
        let durable = Arc::new(RecordingStorage {
            inner: LocalBlobStorage::new(dir.path().to_path_buf()),
            fetched: Default::default(),
        });
        let blob: Arc<dyn BlobStorage> = durable.clone();
        let store = ChunkStore::new(blob);
        let mut entries = Vec::new();
        for i in 0..n_chunks {
            let tag = (i as u8).wrapping_add(1);
            let hash = store
                .put_chunk(&vec![tag; chunk_size as usize])
                .await
                .unwrap();
            entries.push(ChunkRef {
                offset: i as u64 * chunk_size,
                hash,
            });
        }
        let manifest = Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Memory,
            chunk_size: ChunkSize::bytes(chunk_size),
            total_bytes: total,
            chunks: entries,
            parent: None,
            annotations: serde_json::Value::Null,
        };
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let backend = Arc::new(
            ChunkedMemoryBackend::new(
                &manifest,
                &manifest,
                engram_chunk_store::reader::ChunkCacheReader::new(cfg.root.clone()),
                None,
                store,
            )
            .unwrap(),
        );

        // Fake source: seals chunks {1, 3, 5}; chunk 1 = peer bytes,
        // chunk 3 = ZeroChunk, chunk 5 = AltSource (content matches the
        // durable manifest, so the dest fetches it itself).
        let requested = Arc::new(std::sync::Mutex::new(Vec::new()));
        let requests = requested.clone();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let requests = requests.clone();
                std::thread::spawn(move || {
                    let Ok(ToSource::Hello { .. }) = read_frame::<_, ToSource>(&mut s) else {
                        return;
                    };
                    write_frame(
                        &mut s,
                        &FromSource::HelloAck {
                            version: PROTO_VERSION,
                            chunk_size,
                            total_bytes: total,
                        },
                    )
                    .unwrap();
                    let mut bitmap = SealBitmap::new(chunk_size, n_chunks as u64);
                    for i in [1u64, 3, 5].into_iter().chain(8..20) {
                        bitmap.set(i);
                    }
                    write_frame(&mut s, &FromSource::Seal { bitmap }).unwrap();
                    while let Ok(ToSource::NeedAt {
                        req_id,
                        chunk_offset,
                    }) = read_frame::<_, ToSource>(&mut s)
                    {
                        requests.lock().unwrap().push(chunk_offset / chunk_size);
                        let resp = match chunk_offset / chunk_size {
                            1 => {
                                let raw = vec![PEER_BYTE; chunk_size as usize];
                                let (bytes, lz4) = engram_migrate_proto::compress_page(raw);
                                let hash = engram_migrate_proto::wire_hash(&bytes);
                                FromSource::Page {
                                    req_id,
                                    chunk_offset,
                                    bytes,
                                    hash,
                                    lz4,
                                }
                            }
                            3 => FromSource::ZeroChunk {
                                req_id,
                                chunk_offset,
                            },
                            5 | 8..=19 => FromSource::AltSource {
                                req_id,
                                chunk_offset,
                                durable_sha256: [0; 32],
                            },
                            _ => FromSource::Error {
                                req_id: Some(req_id),
                                message: "unsealed".into(),
                            },
                        };
                        if write_frame(&mut s, &resp).is_err() {
                            return;
                        }
                    }
                });
            }
        });

        let len = total as usize;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap failed");
        let uffd = match UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(false)
            .user_mode_only(true)
            .create()
        {
            Ok(u) => u,
            Err(e) => {
                unsafe { libc::munmap(ptr, len) };
                eprintln!(
                    "SKIP: cannot create userfaultfd ({e}); kernel forbids unprivileged uffd"
                );
                return;
            }
        };
        uffd.register(ptr, len).expect("register region with uffd");

        let mappings = vec![GuestRegionUffdMapping {
            base_host_virt_addr: ptr as u64,
            size: len,
            offset: 0,
            page_size: page_size as usize,
        }];
        let session = tokio::task::spawn_blocking(move || {
            crate::peer::PeerSession::connect(
                addr.to_string(),
                "exp".into(),
                "tok".into(),
                chunk_size,
                total,
            )
        })
        .await
        .unwrap()
        .expect("peer connect");
        let session = Arc::new(session);

        let mut rt = Runtime::new(
            mappings,
            uffd,
            backend,
            tokio::runtime::Handle::current(),
            None,
        )
        .unwrap();
        rt.set_peer(Arc::clone(&session), None);
        let rt = Arc::new(rt);

        // Producer order, exactly as run_listener does it: drain, then
        // sweep. Off the tokio workers (block_on inside).
        let rt_drain = Arc::clone(&rt);
        let sess = Arc::clone(&session);
        let kind = hint_kind.to_owned();
        let hot = match hint_kind {
            "installed" => vec![manifest.chunks[1].hash],
            "in-flight" => vec![manifest.chunks[3].hash],
            "unsealed" | "late-unsealed" => {
                vec![manifest.chunks[7].hash, manifest.chunks[6].hash]
            }
            _ => vec![manifest.chunks[19].hash, manifest.chunks[1].hash],
        };
        let stats = std::thread::spawn(move || {
            let mut delivered = false;
            let (tx, mut rx) = tokio::sync::oneshot::channel::<Vec<ChunkHash>>();
            let _pending = if kind == "failed" {
                drop(tx);
                None
            } else {
                Some(tx)
            };
            let stats = rt_drain
                .drain_with_hint(&sess, &[], &mut || {
                    // An unresolved or failed delivery must not gate the first request.
                    if kind == "unavailable" || kind == "failed" {
                        return rx.try_recv().ok();
                    }
                    let ready = if kind == "late-unsealed" {
                        rt_drain.sealed_uninstalled_count(&sess) == 0
                    } else {
                        rt_drain.chunk_installed(1)
                    };
                    if delivered || !ready {
                        return None;
                    }
                    delivered = true;
                    Some(if kind == "empty" {
                        Vec::new()
                    } else {
                        hot.clone()
                    })
                })
                .expect("advisory hints cannot fail the drain");
            rt_drain.mark_drain_done();
            rt_drain.sweep_all().expect("sweep");
            stats
        })
        .join()
        .unwrap();
        assert_eq!(
            (stats.pulled, stats.zero_chunks, stats.alt_sourced),
            (1, 1, 13),
            "drain accounting"
        );

        assert!(
            !session.is_lost(),
            "hint failure must not mark the peer lost"
        );
        let requests = requested.lock().unwrap();
        assert_eq!(requests[0], 1, "drain starts before hint delivery");
        assert_eq!(requests[8], if hint_kind == "delayed" { 19 } else { 13 });
        assert_eq!(
            requests.len(),
            15,
            "installed and in-flight chunks are not requeued"
        );
        assert_eq!(stats.hot_leading, u64::from(hint_kind == "delayed"));
        assert_eq!(
            stats.hot_hint_received,
            matches!(
                hint_kind,
                "delayed" | "installed" | "in-flight" | "unsealed"
            )
        );
        drop(requests);

        // Only unsealed durable reads matter here: sealed AltSource reads
        // happen during the drain. The hint orders only the drain: the
        // sweep reads the unsealed chunks in offset order.
        let unsealed = [0, 2, 4, 6, 7];
        let fetched: Vec<_> = durable
            .fetched
            .lock()
            .unwrap()
            .iter()
            .filter_map(|key| {
                unsealed
                    .iter()
                    .copied()
                    .find(|&i| key == &manifest.chunks[i].hash.storage_key())
            })
            .collect();
        assert_eq!(
            fetched,
            vec![0, 2, 4, 6, 7],
            "unsealed durable fetch order ({hint_kind})"
        );

        // A fault on an already-drained sealed chunk takes the wake
        // path (no peer round-trip, no error).
        let base = ptr as u64;
        rt.serve_pagefault(base + chunk_size)
            .expect("sealed fault post-drain");

        // Every chunk installed; sealed content is the PEER's truth.
        assert!(rt
            .installed
            .lock()
            .unwrap()
            .iter()
            .all(|s| *s == ChunkState::Installed));
        let view = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        for i in 0..n_chunks {
            let start = i * chunk_size as usize;
            let chunk = &view[start..start + chunk_size as usize];
            let want: u8 = match i {
                1 => PEER_BYTE, // peer-authoritative bytes
                3 => 0x00,      // ZeroChunk
                _ => (i as u8).wrapping_add(1), // manifest view (incl. the
                                 // AltSource demote at 5)
            };
            assert!(
                chunk.iter().all(|b| *b == want),
                "chunk {i}: expected {want:#x}"
            );
        }
        unsafe { libc::munmap(ptr, len) };
    }

    /// Regression for issue #227 (b) at the DRAIN level: the source
    /// answers one sealed chunk with `Error { req_id: Some }` (a transient
    /// per-request readv failure) before serving it normally. The drain
    /// must re-request THAT chunk (bounded, FIFO-safe re-queue) and
    /// complete — NOT escalate the whole migration to PeerLost.
    ///
    /// Pre-fix the drain's `decode_page` error → `HandlerError::PeerLost`
    /// arm collapsed every `Error` into a fatal rewind, so a single
    /// transient readv EAGAIN/ENOMEM on the source rewound the whole VM.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn drain_retries_per_request_error_and_does_not_rewind() {
        use engram_migrate_proto::{
            read_frame, write_frame, FromSource, SealBitmap, ToSource, PROTO_VERSION,
        };
        let page_size = 4096u64;
        let chunk_size = page_size;
        let n_chunks = 4usize;
        let total = chunk_size * n_chunks as u64;
        const PEER_BYTE: u8 = 0xC7;

        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = ChunkStore::new(blob);
        let mut entries = Vec::new();
        for i in 0..n_chunks {
            let tag = (i as u8).wrapping_add(1);
            let hash = store
                .put_chunk(&vec![tag; chunk_size as usize])
                .await
                .unwrap();
            entries.push(ChunkRef {
                offset: i as u64 * chunk_size,
                hash,
            });
        }
        let manifest = Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Memory,
            chunk_size: ChunkSize::bytes(chunk_size),
            total_bytes: total,
            chunks: entries,
            parent: None,
            annotations: serde_json::Value::Null,
        };
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let backend = Arc::new(
            ChunkedMemoryBackend::new(
                &manifest,
                &manifest,
                engram_chunk_store::reader::ChunkCacheReader::new(cfg.root.clone()),
                None,
                store,
            )
            .unwrap(),
        );

        // Fake source seals chunk 2 and fails its FIRST NeedAt with a
        // per-request error, then serves it as Page bytes on the retry.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let sealed_idx = 2u64;
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                std::thread::spawn(move || {
                    let Ok(ToSource::Hello { .. }) = read_frame::<_, ToSource>(&mut s) else {
                        return;
                    };
                    write_frame(
                        &mut s,
                        &FromSource::HelloAck {
                            version: PROTO_VERSION,
                            chunk_size,
                            total_bytes: total,
                        },
                    )
                    .unwrap();
                    let mut bitmap = SealBitmap::new(chunk_size, n_chunks as u64);
                    bitmap.set(sealed_idx);
                    write_frame(&mut s, &FromSource::Seal { bitmap }).unwrap();
                    let mut failed_once = false;
                    while let Ok(ToSource::NeedAt {
                        req_id,
                        chunk_offset,
                    }) = read_frame::<_, ToSource>(&mut s)
                    {
                        let resp = if chunk_offset / chunk_size == sealed_idx && !failed_once {
                            failed_once = true;
                            // Per-request failure: conn stays alive.
                            FromSource::Error {
                                req_id: Some(req_id),
                                message: "transient readv EAGAIN".into(),
                            }
                        } else {
                            let raw = vec![PEER_BYTE; chunk_size as usize];
                            let (bytes, lz4) = engram_migrate_proto::compress_page(raw);
                            let hash = engram_migrate_proto::wire_hash(&bytes);
                            FromSource::Page {
                                req_id,
                                chunk_offset,
                                bytes,
                                hash,
                                lz4,
                            }
                        };
                        if write_frame(&mut s, &resp).is_err() {
                            return;
                        }
                    }
                });
            }
        });

        let len = total as usize;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap failed");
        let uffd = match UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(false)
            .user_mode_only(true)
            .create()
        {
            Ok(u) => u,
            Err(e) => {
                unsafe { libc::munmap(ptr, len) };
                eprintln!("SKIP: cannot create userfaultfd ({e})");
                return;
            }
        };
        uffd.register(ptr, len).expect("register region with uffd");
        let mappings = vec![GuestRegionUffdMapping {
            base_host_virt_addr: ptr as u64,
            size: len,
            offset: 0,
            page_size: page_size as usize,
        }];
        let session = tokio::task::spawn_blocking(move || {
            crate::peer::PeerSession::connect(
                addr.to_string(),
                "exp".into(),
                "tok".into(),
                chunk_size,
                total,
            )
        })
        .await
        .unwrap()
        .expect("peer connect");
        let session = Arc::new(session);

        let mut rt = Runtime::new(
            mappings,
            uffd,
            backend,
            tokio::runtime::Handle::current(),
            None,
        )
        .unwrap();
        rt.set_peer(Arc::clone(&session), None);
        let rt = Arc::new(rt);

        let rt_drain = Arc::clone(&rt);
        let sess = Arc::clone(&session);
        let stats = std::thread::spawn(move || rt_drain.drain_from_peer(&sess, &[]))
            .join()
            .unwrap()
            .expect("drain must SUCCEED through the per-request error, not rewind");
        assert_eq!(stats.pulled, 1, "the sealed chunk was pulled after retry");
        assert!(
            !session.is_lost(),
            "a per-request drain error must not latch the peer lost"
        );
        // The sealed chunk holds the PEER's bytes (installed via the retry).
        let view = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        let start = sealed_idx as usize * chunk_size as usize;
        assert!(
            view[start..start + chunk_size as usize]
                .iter()
                .all(|b| *b == PEER_BYTE),
            "sealed chunk must carry the peer bytes after the retry"
        );
        unsafe { libc::munmap(ptr, len) };
    }

    /// Regression for issue #227 (a): once the drain has reported
    /// DrainDone, the source's export may be torn down by
    /// `migration_commit`. A late sealed fault must NOT dial the
    /// (now-gone) peer, park, and fatally escalate PeerLost on a healthy,
    /// fully-drained destination — it must fall through to `resolve()`.
    ///
    /// We model the worst case: `drain_done` latched while a sealed chunk
    /// is (under a hypothetical drain bug) still uninstalled, AND the
    /// source listener is gone. Pre-fix `serve_pagefault` would call
    /// `peer.need_at`, fail to connect, exhaust reconnects, and return
    /// `HandlerError::PeerLost`. Post-fix it skips the peer entirely and
    /// resolves the chunk from the durable manifest.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn sealed_fault_after_drain_done_does_not_dial_torn_down_peer() {
        use engram_migrate_proto::{
            read_frame, write_frame, FromSource, SealBitmap, ToSource, PROTO_VERSION,
        };
        let page_size = 4096u64;
        let chunk_size = page_size;
        let n_chunks = 4usize;
        let total = chunk_size * n_chunks as u64;
        let sealed_idx = 2u64;

        // Manifest carries content for every chunk (the durable view the
        // post-DrainDone fallthrough resolves from).
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = ChunkStore::new(blob);
        let mut entries = Vec::new();
        for i in 0..n_chunks {
            let tag = (i as u8).wrapping_add(1);
            let hash = store
                .put_chunk(&vec![tag; chunk_size as usize])
                .await
                .unwrap();
            entries.push(ChunkRef {
                offset: i as u64 * chunk_size,
                hash,
            });
        }
        let manifest = Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Memory,
            chunk_size: ChunkSize::bytes(chunk_size),
            total_bytes: total,
            chunks: entries,
            parent: None,
            annotations: serde_json::Value::Null,
        };
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let backend = Arc::new(
            ChunkedMemoryBackend::new(
                &manifest,
                &manifest,
                engram_chunk_store::reader::ChunkCacheReader::new(cfg.root.clone()),
                None,
                store,
            )
            .unwrap(),
        );

        // Source: completes the handshake (so `connect` succeeds), then we
        // SHUT IT DOWN to model the export being torn down post-commit.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let accept = std::thread::spawn(move || {
            // The peer dials a Fault conn (`connect` below). Complete the
            // handshake on the first conn that delivers a `Hello`, then park
            // on `stop_rx` until the body tears us down.
            //
            // Bound EVERY blocking operation so this thread can never park
            // forever — the body's `accept.join()` would otherwise hang to
            // the 180 s nextest TIMEOUT (the original failure mode this test
            // tripped on Blacksmith runners, where the previous
            // `continue`-on-bad-read looped straight back into a blocking
            // `accept()` that never returns once the lone dial is consumed).
            // We give the listener an accept deadline and each accepted
            // socket a read timeout; a hiccup or a spurious connect makes us
            // give up (close the listener) so `connect` fails fast and the
            // join always returns, rather than wedging CI.
            listener
                .set_nonblocking(true)
                .expect("listener nonblocking mode");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                let mut s = match listener.accept() {
                    Ok((s, _)) => s,
                    // No pending connection yet: nap briefly and re-poll the
                    // deadline rather than blocking on `accept()` forever.
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => return,
                };
                // Hand the conn back to blocking mode (with a read deadline)
                // for the length-prefixed frame reads.
                s.set_nonblocking(false).expect("conn blocking mode");
                s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .expect("accepted-conn read timeout");
                // Not a Hello (or read timed out / closed): drop this conn and
                // wait for the real dial instead of re-blocking indefinitely.
                let Ok(ToSource::Hello { .. }) = read_frame::<_, ToSource>(&mut s) else {
                    continue;
                };
                write_frame(
                    &mut s,
                    &FromSource::HelloAck {
                        version: PROTO_VERSION,
                        chunk_size,
                        total_bytes: total,
                    },
                )
                .unwrap();
                let mut bitmap = SealBitmap::new(chunk_size, n_chunks as u64);
                bitmap.set(sealed_idx);
                write_frame(&mut s, &FromSource::Seal { bitmap }).unwrap();
                // Hold the seal conn open until told to stop; never serve a
                // NeedAt (the drain isn't run in this test).
                let _ = stop_rx.recv();
                return;
            }
        });

        let len = total as usize;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap failed");
        let uffd = match UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(false)
            .user_mode_only(true)
            .create()
        {
            Ok(u) => u,
            Err(e) => {
                unsafe { libc::munmap(ptr, len) };
                let _ = stop_tx.send(());
                let _ = accept.join();
                eprintln!("SKIP: cannot create userfaultfd ({e})");
                return;
            }
        };
        uffd.register(ptr, len).expect("register region with uffd");
        let mappings = vec![GuestRegionUffdMapping {
            base_host_virt_addr: ptr as u64,
            size: len,
            offset: 0,
            page_size: page_size as usize,
        }];
        let session = tokio::task::spawn_blocking(move || {
            crate::peer::PeerSession::connect(
                addr.to_string(),
                "exp".into(),
                "tok".into(),
                chunk_size,
                total,
            )
        })
        .await
        .unwrap()
        .expect("peer connect");
        let session = Arc::new(session);

        let mut rt = Runtime::new(
            mappings,
            uffd,
            backend,
            tokio::runtime::Handle::current(),
            None,
        )
        .unwrap();
        rt.set_peer(Arc::clone(&session), None);
        let rt = Arc::new(rt);

        // Drain reported complete (latched), and the source export is now
        // gone — exactly the post-commit window.
        rt.mark_drain_done();
        let _ = stop_tx.send(());
        let _ = accept.join();
        // Sanity: the sealed chunk is genuinely still uninstalled here.
        assert!(!rt.chunk_installed(sealed_idx as usize));

        // The sealed fault must resolve WITHOUT dialing the dead peer and
        // WITHOUT escalating PeerLost. `serve_pagefault` may `block_on` the
        // async chunk fetch, so run it on a plain thread (off the tokio
        // workers) and JOIN it directly — exactly like the other
        // `serve_pagefault` tests in this module. We deliberately do NOT
        // wrap it in a `recv_timeout` bound: an in-test timeout that fires
        // would unwind the body and LEAK the still-running fault thread,
        // which keeps the nextest per-test process alive until the 180 s
        // slow-timeout — i.e. it converts a slow path into a permanent CI
        // wedge (the original failure mode of this test). The post-fix gate
        // resolves locally and returns promptly; if it ever genuinely hung,
        // nextest's own per-test slow-timeout is the correct backstop and
        // would attribute the hang to this test directly.
        let base = ptr as u64;
        let rt_f = Arc::clone(&rt);
        let outcome = std::thread::spawn(move || {
            rt_f.serve_pagefault(base + sealed_idx * chunk_size)
                .map_err(|e| e.to_string())
        })
        .join()
        .expect("fault worker panicked");
        assert!(
            outcome.is_ok(),
            "post-DrainDone sealed fault must resolve via the manifest, not PeerLost: {outcome:?}"
        );
        // Resolved from the durable manifest (tag = idx+1), and the peer
        // was never marked lost (no false PeerLost).
        let view = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        let start = sealed_idx as usize * chunk_size as usize;
        let want = (sealed_idx as u8).wrapping_add(1);
        assert!(
            view[start..start + chunk_size as usize]
                .iter()
                .all(|b| *b == want),
            "sealed chunk must be resolved from the durable manifest after DrainDone"
        );
        assert!(
            !session.is_lost(),
            "a fully-drained destination must not be declared PeerLost"
        );
        unsafe { libc::munmap(ptr, len) };
    }

    /// Regression for the cross-region partial-install livelock
    /// (issue #205). FC splits guest RAM into multiple mappings whose
    /// VAs are non-contiguous (the BIOS hole) while the *file* offsets
    /// stay contiguous. A memory chunk straddling a region boundary
    /// used to be `UFFDIO_COPY`'d only up to the first region's tail,
    /// yet marked fully installed — so the spillover into the next
    /// region was never installed by any path and a fault there spun
    /// forever (bit set → wake a still-missing page → refault).
    ///
    /// Layout mirrors `proto::deserializes_multiple_regions`: region A
    /// holds file [0, BOUNDARY) at one VA; region B holds the rest at a
    /// DIFFERENT VA (a gap between them). With a chunk size larger than
    /// BOUNDARY, chunk 0 spans the boundary. We exercise every install
    /// path (copy / zero / drain) and assert BOTH segments land and a
    /// fault on the region-B tail makes progress (no livelock).
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn boundary_spanning_chunk_installs_across_both_regions() {
        let page_size = 4096u64;
        // 2 pages/chunk; the region boundary lands mid-chunk-0 (1 page in).
        let chunk_size = 2 * page_size; // 8192
        let boundary = page_size; // region A = file [0, 4096)
        let total = 3 * page_size; // 12288: chunk0 [0,8192) spans, chunk1 [8192,12288) partial
        let region_b_size = total - boundary; // 8192

        // Plant content: chunk i = byte (i+1) repeated, into a local store.
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = ChunkStore::new(blob);
        let n_chunks = total.div_ceil(chunk_size) as usize; // 2
        let mut entries = Vec::with_capacity(n_chunks);
        for i in 0..n_chunks {
            let off = i as u64 * chunk_size;
            let this_len = std::cmp::min(chunk_size, total - off) as usize;
            let tag = (i as u8).wrapping_add(1);
            let hash = store.put_chunk(&vec![tag; this_len]).await.unwrap();
            entries.push(ChunkRef { offset: off, hash });
        }
        let manifest = Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Memory,
            chunk_size: ChunkSize::bytes(chunk_size),
            total_bytes: total,
            chunks: entries,
            parent: None,
            annotations: serde_json::Value::Null,
        };
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let backend = Arc::new(
            ChunkedMemoryBackend::new(
                &manifest,
                &manifest,
                engram_chunk_store::reader::ChunkCacheReader::new(cfg.root.clone()),
                None,
                store,
            )
            .unwrap(),
        );

        // Two SEPARATE mmaps → naturally non-contiguous VAs (the gap),
        // contiguous file offsets via the `offset` field. This is the
        // production layout the single-region tests never reproduced.
        let map = |sz: usize| -> *mut std::ffi::c_void {
            unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    sz,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            }
        };
        let ptr_a = map(boundary as usize);
        let ptr_b = map(region_b_size as usize);
        assert_ne!(ptr_a, libc::MAP_FAILED, "mmap A failed");
        assert_ne!(ptr_b, libc::MAP_FAILED, "mmap B failed");

        let uffd = match UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(false)
            .user_mode_only(true)
            .create()
        {
            Ok(u) => u,
            Err(e) => {
                unsafe { libc::munmap(ptr_a, boundary as usize) };
                unsafe { libc::munmap(ptr_b, region_b_size as usize) };
                eprintln!(
                    "SKIP: cannot create userfaultfd ({e}); kernel forbids unprivileged uffd"
                );
                return;
            }
        };
        uffd.register(ptr_a, boundary as usize)
            .expect("register region A");
        uffd.register(ptr_b, region_b_size as usize)
            .expect("register region B");

        let mappings = vec![
            GuestRegionUffdMapping {
                base_host_virt_addr: ptr_a as u64,
                size: boundary as usize,
                offset: 0,
                page_size: page_size as usize,
            },
            GuestRegionUffdMapping {
                base_host_virt_addr: ptr_b as u64,
                size: region_b_size as usize,
                offset: boundary, // file offsets stay contiguous
                page_size: page_size as usize,
            },
        ];
        let rt = Arc::new(
            Runtime::new(
                mappings,
                uffd,
                backend,
                tokio::runtime::Handle::current(),
                None,
            )
            .unwrap(),
        );

        // Fault on region A (file offset 0) → the fault loop resolves
        // chunk 0 and installs it. chunk 0's file range [0, 8192) spans
        // the boundary @ 4096. block_on inside serve_pagefault → run it
        // on a plain thread, exactly like the handler's blocking loop.
        let rt_i = Arc::clone(&rt);
        let ptr_a_addr = ptr_a as u64;
        std::thread::spawn(move || {
            rt_i.serve_pagefault(ptr_a_addr)
                .expect("install chunk 0 via fault on region A");
        })
        .join()
        .unwrap();

        // Region A (file [0,4096)) AND region B's first page (file
        // [4096,8192)) must both hold chunk-0's content (tag 1). Before
        // the fix, region B's segment was never installed.
        let view_a = unsafe { std::slice::from_raw_parts(ptr_a as *const u8, boundary as usize) };
        assert!(
            view_a.iter().all(|b| *b == 1),
            "region A segment of chunk 0 must be installed"
        );
        let view_b =
            unsafe { std::slice::from_raw_parts(ptr_b as *const u8, region_b_size as usize) };
        assert!(
            view_b[0..page_size as usize].iter().all(|b| *b == 1),
            "region B segment of chunk 0 must be installed (was the livelock bug)"
        );

        // A fault on the region-B tail of chunk 0 (file offset 4096)
        // must make progress: chunk already installed → wake, NOT spin
        // on a missing page. (The page is present now, so wake returns
        // cleanly; pre-fix the page was missing and the vCPU would
        // refault forever.) Then serve the partial chunk 1, which lives
        // entirely in region B at file [8192,12288) → VA ptr_b +
        // (8192-4096): the "fault rounds to chunk start in region B" path.
        // Both off the tokio workers (serve_pagefault may block_on).
        let rt_f = Arc::clone(&rt);
        let ptr_b_addr = ptr_b as u64;
        let tail_off = chunk_size - boundary;
        std::thread::spawn(move || {
            rt_f.serve_pagefault(ptr_b_addr)
                .expect("fault on region-B tail must make progress");
            rt_f.serve_pagefault(ptr_b_addr + tail_off)
                .expect("serve chunk 1");
        })
        .join()
        .unwrap();
        assert!(
            view_b[(chunk_size - boundary) as usize..]
                .iter()
                .all(|b| *b == 2),
            "chunk 1 (region-B-only) content"
        );

        assert!(
            rt.installed
                .lock()
                .unwrap()
                .iter()
                .all(|s| *s == ChunkState::Installed),
            "every chunk installed"
        );

        unsafe { libc::munmap(ptr_a, boundary as usize) };
        unsafe { libc::munmap(ptr_b, region_b_size as usize) };
    }

    // ---------------------------------------------------------------
    // issue #206: the per-chunk install bit was set BEFORE the fallible
    // fetch/copy and never rolled back, permanently poisoning a chunk on
    // any transient error (silent guest hang). The tri-state install
    // lifecycle (`Empty/Installing/Installed`) makes the install atomic
    // (publish `Installed` only on full success, roll back to `Empty` on
    // error) and serialised (a racing fault WAITS on an in-flight
    // install instead of busy-spinning a missing page). The next four
    // tests cover the issue's acceptance criteria. They drive the
    // install-state machine directly so they run unconditionally in CI
    // (no privileged userfaultfd required); the end-to-end retry through
    // a real UFFD is covered by `fetch_failure_rolls_back_then_retries`.

    /// Several fault threads serving faults on the same chunks: each chunk
    /// is installed by exactly one of them, and the rest only wake.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fault_threads_on_overlapping_faults_install_each_chunk_once() {
        let n_chunks = 8usize;
        let chunk_size = 4 * 4096u64;
        let Some(rt) = test_runtime_with_chunk_size(n_chunks, chunk_size) else {
            return;
        };
        let base = rt.mappings()[0].base_host_virt_addr;
        let pages = n_chunks as u64 * chunk_size / 4096;
        let threads: Vec<_> = (0..4u64)
            .map(|t| {
                let rt = Arc::clone(&rt);
                std::thread::spawn(move || {
                    (0..pages)
                        .map(|p| {
                            // Each thread walks the pages from its own start.
                            let page = (p + t * 3) % pages;
                            let source = rt
                                .serve_pagefault_classified(base + page * 4096)
                                .expect("serve fault");
                            ((page * 4096 / chunk_size) as usize, source)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut installs = vec![0usize; n_chunks];
        for t in threads {
            for (idx, source) in t.join().unwrap() {
                if source != Source::Wake {
                    installs[idx] += 1;
                }
            }
        }
        assert_eq!(installs, vec![1; n_chunks]);
    }

    /// Build a small runtime with a real anonymous UFFD mapping, for
    /// install-state tests and demand-window tests. Returns None when
    /// the kernel forbids userfaultfd. Tests that use the mapping must
    /// keep it live until their reader and installer threads have joined.
    fn bookkeeping_runtime(n_chunks: usize) -> Option<Arc<Runtime>> {
        test_runtime_with_chunk_size(n_chunks, 4096)
    }

    fn test_runtime_with_chunk_size(n_chunks: usize, chunk_size: u64) -> Option<Arc<Runtime>> {
        let page_size = 4096u64;
        let total = chunk_size * n_chunks as u64;

        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = ChunkStore::new(blob);
        // Manifest of all-zero (canonical-omitted) chunks: total_bytes
        // only needs to match the mapping sum; no chunk content is
        // fetched by these bookkeeping-only tests.
        let manifest = Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Memory,
            chunk_size: ChunkSize::bytes(chunk_size),
            total_bytes: total,
            chunks: Vec::new(),
            parent: None,
            annotations: serde_json::Value::Null,
        };
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let backend = Arc::new(
            ChunkedMemoryBackend::new(
                &manifest,
                &manifest,
                engram_chunk_store::reader::ChunkCacheReader::new(cfg.root.clone()),
                None,
                store,
            )
            .unwrap(),
        );

        let len = total as usize;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap failed");
        let uffd = match UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(false)
            .user_mode_only(true)
            .create()
        {
            Ok(u) => u,
            Err(e) => {
                unsafe { libc::munmap(ptr, len) };
                eprintln!("SKIP: cannot create userfaultfd ({e})");
                return None;
            }
        };
        uffd.register(ptr, len).expect("register region with uffd");
        let mappings = vec![GuestRegionUffdMapping {
            base_host_virt_addr: ptr as u64,
            size: len,
            offset: 0,
            page_size: page_size as usize,
        }];
        Some(Arc::new(
            Runtime::new(
                mappings,
                uffd,
                backend,
                tokio::runtime::Handle::current(),
                None,
            )
            .unwrap(),
        ))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_suffix_does_not_block_another_fault_or_drain_and_late_reply_is_dropped() {
        use crate::peer::{
            tests::{fake_source, page_resp},
            PeerSession,
        };
        let Some(mut rt) = bookkeeping_runtime(4) else {
            return;
        };
        let (stalled_tx, stalled_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let stalled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let addr = fake_source(vec![0, 1, 2, 3], move |req, offset| {
            if offset == 4096 && !stalled.swap(true, std::sync::atomic::Ordering::SeqCst) {
                stalled_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
                // This invalid late frame must not mark a drained peer lost.
                return engram_migrate_proto::FromSource::ZeroChunk {
                    req_id: req,
                    chunk_offset: 0,
                };
            }
            page_resp(req, offset)
        });
        let peer = tokio::task::spawn_blocking(move || {
            PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), 4096, 16384)
        })
        .await
        .unwrap()
        .unwrap();
        let peer = Arc::new(peer);
        let mutable = Arc::get_mut(&mut rt).unwrap();
        mutable.fault_around_chunks = 2;
        mutable.set_peer(peer.clone(), None);
        let base = rt.mappings[0].base_host_virt_addr;
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let fault_rt = rt.clone();
        let first =
            std::thread::spawn(move || done_tx.send(fault_rt.serve_pagefault(base)).unwrap());
        stalled_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first fault waited for suffix")
            .unwrap();
        first.join().unwrap();
        assert!(!peer.fault_active_within(Duration::ZERO));
        assert_eq!(
            rt.fault_around_chunks_installed
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let fault_rt = rt.clone();
        let second = std::thread::spawn(move || {
            done_tx.send(fault_rt.serve_pagefault(base + 8192)).unwrap()
        });
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second fault waited for suffix")
            .unwrap();
        second.join().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let drain_rt = rt.clone();
        let drain_peer = peer.clone();
        let drain = std::thread::spawn(move || {
            let result = drain_rt.drain_from_peer(&drain_peer, &[]);
            drain_rt.mark_drain_done();
            done_tx.send(result).unwrap();
        });
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("drain waited for suffix")
            .unwrap();
        drain.join().unwrap();
        let count = rt
            .fault_around_chunks_installed
            .load(std::sync::atomic::Ordering::Relaxed);
        release_tx.send(()).unwrap();
        assert!(!peer.is_lost());
        assert_eq!(
            rt.fault_around_chunks_installed
                .load(std::sync::atomic::Ordering::Relaxed),
            count
        );
        assert!((0..4).all(|i| rt.chunk_installed(i)));
        // SAFETY: DrainDone fenced all later installs; no installer remains.
        unsafe {
            libc::munmap(base as *mut _, 16384);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn overlapping_and_unrelated_faults_and_drain_bypass_stalled_suffix() {
        use crate::peer::{
            tests::{fake_source, page_resp},
            PeerSession,
        };
        let Some(mut rt) = bookkeeping_runtime(4) else {
            return;
        };
        let (stalled_tx, stalled_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let stalled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let source_seen = seen.clone();
        let addr = fake_source(vec![0, 1, 2, 3], move |req, offset| {
            source_seen.lock().unwrap().push(offset);
            if offset == 4096 && !stalled.swap(true, std::sync::atomic::Ordering::SeqCst) {
                stalled_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
            page_resp(req, offset)
        });
        let peer = tokio::task::spawn_blocking(move || {
            PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), 4096, 16384)
        })
        .await
        .unwrap()
        .unwrap();
        let peer = Arc::new(peer);
        let mutable = Arc::get_mut(&mut rt).unwrap();
        mutable.fault_around_chunks = 3;
        mutable.set_peer(peer.clone(), None);
        let base = rt.mappings[0].base_host_virt_addr;
        // Exercise the same serial fault loop: A, overlapping C, unrelated D.
        // B stays blocked throughout all three faults and the drain.
        for offset in [0, 8192, 12288] {
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let fault_rt = rt.clone();
            let fault = std::thread::spawn(move || {
                done_tx
                    .send(fault_rt.serve_pagefault(base + offset))
                    .unwrap();
            });
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("demand waited for speculative B")
                .unwrap();
            fault.join().unwrap();
            if offset == 0 {
                stalled_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            assert!(!peer.fault_active_within(Duration::ZERO));
        }
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let drain_rt = rt.clone();
        let drain_peer = peer.clone();
        let drain = std::thread::spawn(move || {
            done_tx
                .send(drain_rt.drain_from_peer(&drain_peer, &[]))
                .unwrap();
        });
        let stats = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("drain waited for speculative B")
            .unwrap();
        drain.join().unwrap();
        assert_eq!(stats.pulled, 1);
        assert!((0..4).all(|i| rt.chunk_installed(i)));
        assert_eq!(
            rt.fault_around_chunks_installed
                .load(std::sync::atomic::Ordering::Relaxed),
            3
        );

        // Let the worker attempt both duplicate installs, without cancellation.
        release_tx.send(()).unwrap();
        rt.finish_fault_window();
        assert_eq!(
            rt.fault_around_chunks_installed
                .load(std::sync::atomic::Ordering::Relaxed),
            3
        );
        let mut requests = seen.lock().unwrap().clone();
        requests.sort_unstable();
        assert_eq!(requests, [0, 4096, 4096, 8192, 8192, 12288]);
        assert!(!peer.is_lost());
        // SAFETY: all faults, the drain, and the prefetch worker have joined.
        unsafe {
            let bytes = std::slice::from_raw_parts(base as *const u8, 16384);
            assert!(bytes.iter().all(|byte| *byte == 0xAB));
            libc::munmap(base as *mut _, 16384);
        }
    }

    #[test]
    fn fault_window_stop_detaches_a_worker_that_misses_the_deadline() {
        let progress = Arc::new(WindowProgress::new());
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (exited_tx, exited_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            release_rx.recv().unwrap();
            exited_tx.send(()).unwrap();
        });
        let job = FaultWindow {
            progress: progress.clone(),
            thread,
        };
        let started = std::time::Instant::now();
        job.stop();
        assert!(started.elapsed() < crate::peer::PEER_READ_TIMEOUT + Duration::from_secs(1));
        assert!(progress.stopped());
        release_tx.send(()).unwrap();
        exited_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn fault_window_stop_deadline_includes_install_lock() {
        let progress = Arc::new(WindowProgress::new());
        let worker_progress = progress.clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (exited_tx, exited_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _guard = worker_progress.socket.lock().unwrap();
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            assert!(worker_progress.stopped());
            exited_tx.send(()).unwrap();
        });
        locked_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let job = FaultWindow {
            progress: progress.clone(),
            thread,
        };
        let started = std::time::Instant::now();
        job.stop();
        let elapsed = started.elapsed();
        // Release even when the timing assertion fails.
        release_tx.send(()).unwrap();
        exited_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(elapsed < crate::peer::PEER_READ_TIMEOUT + Duration::from_secs(1));
        assert!(progress.stopped());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_alt_source_does_not_block_stop_or_install_after_cancellation() {
        use crate::peer::{
            tests::{fake_source, page_resp},
            PeerSession,
        };
        use engram_substrate_proto::{read_frame, write_frame, FromWriter, ToWriter};
        use std::os::fd::AsFd;
        use std::os::unix::net::UnixListener;

        // Both late success and late failure must be discarded.
        for fail in [false, true] {
            let Some(mut rt) = bookkeeping_runtime(2) else {
                return;
            };
            let dir = tempfile::tempdir().unwrap();
            let blob: Arc<dyn BlobStorage> =
                Arc::new(LocalBlobStorage::new(dir.path().join("blob")));
            let store = ChunkStore::new(blob);
            let bytes = vec![0xCD; 4096];
            let hash = store.put_chunk(&bytes).await.unwrap();
            let manifest = Manifest {
                schema_version: MANIFEST_SCHEMA_VERSION,
                kind: ManifestKind::Memory,
                chunk_size: ChunkSize::bytes(4096),
                total_bytes: 8192,
                chunks: vec![ChunkRef { offset: 4096, hash }],
                parent: None,
                annotations: serde_json::Value::Null,
            };
            let path = dir.path().join("populate.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let file_path = dir.path().join("chunk");
            std::fs::write(&file_path, bytes).unwrap();
            let (fetch_tx, fetch_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let durable = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                let request: ToWriter = read_frame(&mut socket).unwrap();
                assert!(matches!(request, ToWriter::Populate { .. }));
                fetch_tx.send(()).unwrap();
                // No answer until stop has returned. This is below the client's
                // 15-second timeout, so the worker cannot finish on its own.
                release_rx.recv().unwrap();
                if fail {
                    write_frame(
                        &mut socket,
                        &FromWriter::PopulateErr {
                            msg: "durable source failed".into(),
                        },
                    )
                    .unwrap();
                } else {
                    write_frame(&mut socket, &FromWriter::Populated { len: 4096 }).unwrap();
                    let file = std::fs::File::open(file_path).unwrap();
                    engram_substrate_proto::send_fd(socket.as_fd(), file.as_fd()).unwrap();
                }
            });
            Arc::get_mut(&mut rt).unwrap().backend = Arc::new(
                ChunkedMemoryBackend::new(
                    &manifest,
                    &manifest,
                    engram_chunk_store::reader::ChunkCacheReader::new(dir.path().join("cache")),
                    Some(Arc::new(crate::populate_client::PopulateClient::new(path))),
                    store,
                )
                .unwrap(),
            );
            let addr = fake_source(vec![0, 1], move |req, offset| {
                if offset == 4096 {
                    engram_migrate_proto::FromSource::AltSource {
                        req_id: req,
                        chunk_offset: offset,
                        durable_sha256: *hash.as_bytes(),
                    }
                } else {
                    page_resp(req, offset)
                }
            });
            let peer = Arc::new(
                tokio::task::spawn_blocking(move || {
                    PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), 4096, 8192)
                })
                .await
                .unwrap()
                .unwrap(),
            );
            Arc::get_mut(&mut rt).unwrap().set_peer(peer.clone(), None);
            let base = rt.mappings[0].base_host_virt_addr;
            let fault_rt = rt.clone();
            std::thread::spawn(move || fault_rt.serve_pagefault(base))
                .join()
                .unwrap()
                .unwrap();
            fetch_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let progress = rt
                .fault_window
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .progress
                .clone();
            assert!(
                progress.socket.try_lock().is_ok(),
                "durable fetch held install lock"
            );
            assert_eq!(Arc::strong_count(&rt), 1, "fetch retained runtime mappings");
            let started = std::time::Instant::now();
            rt.mark_drain_done();
            let elapsed = started.elapsed();
            assert!(progress.stopped());
            assert!(rt.is_drain_done());
            release_tx.send(()).unwrap();
            durable.join().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while Arc::strong_count(&progress) != 1 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "detached worker did not exit"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(elapsed < crate::peer::PEER_READ_TIMEOUT + Duration::from_secs(1));
            assert!(!rt.chunk_installed(1));
            assert_eq!(rt.fault_around_chunks_installed.load(Ordering::Relaxed), 1);
            assert!(!peer.is_lost());
            // SAFETY: the fault and detached prefetch worker have exited.
            unsafe {
                libc::munmap(base as *mut _, 8192);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn demand_window_installs_all_shapes_and_wakes_only_faulting_page() {
        use crate::peer::{tests::fake_source_geometry, PeerSession};
        let Some(mut rt) = test_runtime_with_chunk_size(4, 8192) else {
            return;
        };
        let addr =
            fake_source_geometry(vec![0, 1, 2, 3], 8192, 32768, |req, offset| match offset {
                8192 => engram_migrate_proto::FromSource::ZeroChunk {
                    req_id: req,
                    chunk_offset: offset,
                },
                16384 => engram_migrate_proto::FromSource::AltSource {
                    req_id: req,
                    chunk_offset: offset,
                    durable_sha256: [0; 32],
                },
                _ => {
                    let (bytes, lz4) = engram_migrate_proto::compress_page(vec![0xAB; 8192]);
                    let hash = engram_migrate_proto::wire_hash(&bytes);
                    engram_migrate_proto::FromSource::Page {
                        req_id: req,
                        chunk_offset: offset,
                        bytes,
                        hash,
                        lz4,
                    }
                }
            });
        let peer = tokio::task::spawn_blocking(move || {
            PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), 8192, 32768)
        })
        .await
        .unwrap()
        .unwrap();
        Arc::get_mut(&mut rt)
            .unwrap()
            .set_peer(Arc::new(peer), None);
        let base = rt.mappings[0].base_host_virt_addr;
        // Queue faults in the first chunk and the next chunk before the install.
        let (tx, rx) = std::sync::mpsc::channel();
        let readers: Vec<_> = [0, 4096, 8192]
            .into_iter()
            .map(|offset| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    // SAFETY: the registered mapping remains live until all readers join.
                    let byte = unsafe { std::ptr::read_volatile((base + offset) as *const u8) };
                    tx.send((offset, byte)).unwrap();
                })
            })
            .collect();
        for _ in 0..3 {
            assert!(matches!(
                rt.uffd.read_event().unwrap(),
                Some(Event::Pagefault { .. })
            ));
        }
        let serve = rt.clone();
        std::thread::spawn(move || serve.serve_pagefault(base))
            .join()
            .unwrap()
            .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), (0, 0xAB));
        assert!(
            rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "adjacent page was woken"
        );
        rt.finish_fault_window();
        assert!((0..4).all(|i| rt.chunk_installed(i)));
        assert_eq!(
            rt.fault_around_chunks_installed
                .load(std::sync::atomic::Ordering::Relaxed),
            4
        );
        rt.serve_pagefault(base + 4096).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            (4096, 0xAB)
        );
        rt.serve_pagefault(base + 8192).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), (8192, 0));
        for reader in readers {
            reader.join().unwrap();
        }
        // SAFETY: no reader or installer still uses the mapping.
        unsafe {
            libc::munmap(base as *mut _, 32768);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn demand_window_deduplicates_concurrent_drain_install() {
        use crate::peer::{
            tests::{fake_source, page_resp},
            PeerSession,
        };
        let Some(mut rt) = bookkeeping_runtime(4) else {
            return;
        };
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let source_barrier = barrier.clone();
        let addr = fake_source(vec![0, 1, 2, 3], move |req, offset| {
            if offset == 4096 {
                source_barrier.wait();
                source_barrier.wait();
            }
            page_resp(req, offset)
        });
        let peer = tokio::task::spawn_blocking(move || {
            PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), 4096, 16384)
        })
        .await
        .unwrap()
        .unwrap();
        Arc::get_mut(&mut rt)
            .unwrap()
            .set_peer(Arc::new(peer), None);
        let base = rt.mappings[0].base_host_virt_addr;
        let fault_rt = rt.clone();
        let fault = std::thread::spawn(move || fault_rt.serve_pagefault(base));
        barrier.wait();
        assert!(rt
            .install(
                4096,
                Fill::Private(Payload::Bytes(Bytes::from(vec![0xCD; 4096]))),
                Installer::Sweep,
            )
            .unwrap());
        barrier.wait();
        fault.join().unwrap().unwrap();
        rt.finish_fault_window();
        assert_eq!(
            rt.fault_around_chunks_installed
                .load(std::sync::atomic::Ordering::Relaxed),
            3
        );
        // SAFETY: all chunks are installed, and the fault worker has joined.
        assert_eq!(unsafe { std::ptr::read((base + 4096) as *const u8) }, 0xCD);
        unsafe {
            libc::munmap(base as *mut _, 16384);
        }
    }

    /// A chunk another request holds is not asked for again by the drain,
    /// and every peer install lands in the sealed output, which gets its
    /// final name only when the caller finishes it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_skips_a_requested_chunk_and_records_the_sealed_output() {
        use crate::peer::{
            tests::{fake_source, page_resp},
            PeerSession, SealedOutput,
        };
        let Some(mut rt) = bookkeeping_runtime(4) else {
            return;
        };
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        let addr = fake_source(vec![0, 1, 2, 3], move |req, offset| {
            log.lock().unwrap().push(offset);
            page_resp(req, offset)
        });
        let peer = tokio::task::spawn_blocking(move || {
            PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), 4096, 16384)
        })
        .await
        .unwrap()
        .unwrap();
        let peer = Arc::new(peer);
        let out_dir = tempfile::tempdir().unwrap();
        let out_path = out_dir.path().join("sealed.bin");
        {
            let rt = Arc::get_mut(&mut rt).unwrap();
            rt.set_peer(peer.clone(), None);
            rt.set_sealed_output(SealedOutput::create(out_path.clone()).unwrap());
            // Long enough that the drain waits for this test's install.
            rt.peer_requested_wait = Duration::from_secs(10);
        }
        // A fault window holds chunk 1.
        assert!(rt.try_request(1));
        let drain_rt = rt.clone();
        let drain_peer = peer.clone();
        let drain = std::thread::spawn(move || drain_rt.drain_from_peer(&drain_peer, &[]));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while ![0, 2, 3].iter().all(|i| rt.chunk_installed(*i)) {
            assert!(
                std::time::Instant::now() < deadline,
                "drain did not install"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(rt
            .install(
                4096,
                Fill::Private(Payload::Bytes(Bytes::from(vec![0x5A; 4096]))),
                Installer::Peer,
            )
            .unwrap());
        rt.clear_request(1);
        let stats = drain.join().unwrap().unwrap();
        assert_eq!(stats.pulled, 3);
        let mut asked = asked.lock().unwrap().clone();
        asked.sort_unstable();
        assert_eq!(
            asked,
            vec![0, 8192, 12288],
            "the held chunk was asked for again"
        );
        assert!(!out_path.exists(), "an unfinished set has no final name");
        rt.sealed_out.as_ref().unwrap().finish();
        let sealed = std::fs::read(&out_path).unwrap();
        assert_eq!(sealed.len(), 16384);
        assert!(sealed[..4096].iter().all(|b| *b == 0xAB));
        assert!(sealed[4096..8192].iter().all(|b| *b == 0x5A));
        assert!(sealed[8192..].iter().all(|b| *b == 0xAB));
        let base = rt.mappings[0].base_host_virt_addr;
        // SAFETY: the drain has joined and no fault is in flight.
        unsafe {
            libc::munmap(base as *mut _, 16384);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn demand_window_skips_unsealed_and_installed_chunks() {
        use crate::peer::{
            tests::{fake_source, page_resp},
            PeerSession,
        };
        let Some(mut rt) = bookkeeping_runtime(4) else {
            return;
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let source_seen = seen.clone();
        let addr = fake_source(vec![0, 1, 3], move |req, offset| {
            source_seen.lock().unwrap().push(offset);
            page_resp(req, offset)
        });
        let peer = tokio::task::spawn_blocking(move || {
            PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), 4096, 16384)
        })
        .await
        .unwrap()
        .unwrap();
        Arc::get_mut(&mut rt)
            .unwrap()
            .set_peer(Arc::new(peer), None);
        assert!(rt
            .install(
                4096,
                Fill::Private(Payload::Bytes(Bytes::from(vec![0xCD; 4096]))),
                Installer::Sweep,
            )
            .unwrap());
        let base = rt.mappings[0].base_host_virt_addr;
        let fault_rt = rt.clone();
        std::thread::spawn(move || fault_rt.serve_pagefault(base))
            .join()
            .unwrap()
            .unwrap();
        rt.finish_fault_window();
        assert_eq!(*seen.lock().unwrap(), vec![0, 12288]);
        assert!(!rt.chunk_installed(2));
        // SAFETY: the fault worker has joined and no thread uses the mapping.
        unsafe {
            libc::munmap(base as *mut _, 16384);
        }
    }

    /// Read the control backlog after the worker has exited. Any earlier
    /// PeerLost must appear before DrainDone and fail this assertion.
    fn assert_window_drain_done(rt: &Runtime, control_path: &std::path::Path) {
        use engram_migrate_proto::{read_frame, HandlerControl};
        rt.mark_drain_done();
        rt.control
            .as_ref()
            .unwrap()
            .report(HandlerControl::DrainDone {
                pulled: 0,
                alt_sourced: 0,
                zero_chunks: 0,
                ms: 0,
                faults: 0,
                fault_us: 0,
                fault_max_us: 0,
                fault_around_chunks_installed: 0,
            });
        let mut subscriber = std::os::unix::net::UnixStream::connect(control_path).unwrap();
        subscriber
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert!(matches!(
            read_frame::<_, HandlerControl>(&mut subscriber).unwrap(),
            HandlerControl::DrainDone { .. }
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn demand_window_first_read_timeout_reconnects_before_first_response_deadline() {
        use crate::peer::{
            tests::{fake_source, page_resp},
            ControlTx, PeerSession,
        };
        // The single-chunk setting must retain the same retry policy.
        for window in [1, 2] {
            let Some(mut rt) = bookkeeping_runtime(4) else {
                return;
            };
            let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let source_requests = requests.clone();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let release_rx = Arc::new(Mutex::new(release_rx));
            let addr = fake_source(vec![0], move |req, offset| {
                if source_requests.fetch_add(1, Ordering::SeqCst) == 0 {
                    // Only a read timeout can start the retry: keep the first
                    // connection open without sending any response bytes.
                    let _ = release_rx.lock().unwrap().recv();
                }
                page_resp(req, offset)
            });
            let peer = Arc::new(
                PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), 4096, 16384)
                    .unwrap(),
            );
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("control.sock");
            let control = Arc::new(ControlTx::bind(&path).unwrap());
            let mutable = Arc::get_mut(&mut rt).unwrap();
            mutable.fault_around_chunks = window;
            mutable.set_peer(peer.clone(), Some(control));
            let base = rt.mappings[0].base_host_virt_addr;
            let fault_rt = rt.clone();
            let result = std::thread::spawn(move || fault_rt.serve_pagefault(base))
                .join()
                .unwrap();
            release_tx.send(()).unwrap();
            result.unwrap();
            rt.finish_fault_window();
            assert_eq!(requests.load(Ordering::SeqCst), 2);
            assert!(rt.chunk_installed(0));
            assert!(!peer.is_lost());
            assert_window_drain_done(&rt, &path);
            // SAFETY: the fault and prefetch worker have exited.
            unsafe {
                libc::munmap(base as *mut _, 16384);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn obsolete_suffix_failure_before_cancellation_does_not_report_peer_lost() {
        use crate::peer::{
            tests::{fake_source, page_resp},
            ControlTx, PeerSession,
        };
        let Some(mut rt) = bookkeeping_runtime(4) else {
            return;
        };
        let (pending_tx, pending_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let addr = fake_source(vec![0, 1], move |req, offset| {
            if offset == 4096 {
                pending_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
                engram_migrate_proto::FromSource::Error {
                    req_id: None,
                    message: "obsolete suffix failed".into(),
                }
            } else {
                page_resp(req, offset)
            }
        });
        let peer = Arc::new(
            PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), 4096, 16384).unwrap(),
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let control = Arc::new(ControlTx::bind(&path).unwrap());
        Arc::get_mut(&mut rt)
            .unwrap()
            .set_peer(peer.clone(), Some(control));
        let base = rt.mappings[0].base_host_virt_addr;
        let fault_rt = rt.clone();
        std::thread::spawn(move || fault_rt.serve_pagefault(base))
            .join()
            .unwrap()
            .unwrap();
        pending_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(rt
            .install(
                4096,
                Fill::Private(Payload::Bytes(Bytes::from(vec![0xCD; 4096]))),
                Installer::Sweep,
            )
            .unwrap());
        assert_eq!(rt.sealed_uninstalled_count(&peer), 0);
        assert!(!rt.is_drain_done());
        assert!(!rt
            .fault_window
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .progress
            .stopped());
        release_tx.send(()).unwrap();
        // Let the error reach publication BEFORE mark_drain_done cancels work.
        rt.finish_fault_window();
        assert!(!peer.is_lost());
        assert_window_drain_done(&rt, &path);
        // SAFETY: the fault and prefetch worker have exited.
        unsafe {
            libc::munmap(base as *mut _, 16384);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn demand_window_error_preserves_installed_prefix_and_reports_peer_lost() {
        use crate::peer::{
            tests::{fake_source, page_resp},
            PeerSession,
        };
        let Some(mut rt) = bookkeeping_runtime(4) else {
            return;
        };
        let addr = fake_source(vec![0, 1, 2, 3], |req, offset| {
            if offset == 4096 {
                engram_migrate_proto::FromSource::Error {
                    req_id: None,
                    message: "source lost".into(),
                }
            } else {
                page_resp(req, offset)
            }
        });
        let peer = tokio::task::spawn_blocking(move || {
            PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), 4096, 16384)
        })
        .await
        .unwrap()
        .unwrap();
        Arc::get_mut(&mut rt)
            .unwrap()
            .set_peer(Arc::new(peer), None);
        let base = rt.mappings[0].base_host_virt_addr;
        let fault_rt = rt.clone();
        std::thread::spawn(move || fault_rt.serve_pagefault(base))
            .join()
            .unwrap()
            .unwrap();
        rt.finish_fault_window();
        assert!(rt.chunk_installed(0));
        assert!(!rt.chunk_installed(1));
        assert!(rt.peer.as_ref().unwrap().is_lost());
        // SAFETY: the fault worker has joined and no thread uses the mapping.
        unsafe {
            libc::munmap(base as *mut _, 16384);
        }
    }

    /// Acceptance #2 + #1 (state level): a claim that ends in error rolls
    /// the chunk back to `Empty` so the SAME chunk can be re-claimed and
    /// retried — the bit is never left set across a failure, which is the
    /// whole #206 bug (before the fix the second claim would see the bit
    /// set forever and short-circuit to a never-resolving wake).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_install_rolls_back_and_chunk_is_reclaimable() {
        let Some(rt) = bookkeeping_runtime(2) else {
            return;
        };
        // First installer claims chunk 0.
        assert_eq!(rt.claim_chunk(0), Claim::Claimed);
        assert!(!rt.chunk_installed(0), "Installing is not yet serveable");
        // Its install fails -> rollback to Empty.
        rt.release_chunk(0);
        assert!(!rt.chunk_installed(0));
        // The chunk is retryable: a later fault re-claims it (NOT `Done`,
        // which would short-circuit to a wake on a never-populated page).
        assert_eq!(
            rt.claim_chunk(0),
            Claim::Claimed,
            "a rolled-back chunk must be re-claimable, not permanently poisoned"
        );
        // Success this time publishes Installed; further claims are Done.
        rt.mark_installed(0);
        assert!(rt.chunk_installed(0));
        assert_eq!(rt.claim_chunk(0), Claim::Done);
    }

    /// Acceptance #3: a fault that races an in-flight install must
    /// TERMINATE once the install settles — never busy-spin. Here the
    /// installer holds `Installing`, the waiter blocks in
    /// `claim_for_install` (no spin), and on the installer's SUCCESS the
    /// waiter resolves to `Done` (wake its page).
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn concurrent_fault_during_inflight_install_waits_then_resolves() {
        let Some(rt) = bookkeeping_runtime(1) else {
            return;
        };
        // Installer claims chunk 0 and is "mid-flight".
        assert_eq!(rt.claim_chunk(0), Claim::Claimed);

        let rt_w = Arc::clone(&rt);
        let waiter = std::thread::spawn(move || {
            // Must block until the installer settles, then return Done
            // (installed) — NOT spin, NOT re-claim.
            rt_w.claim_for_install(0)
        });

        // Give the waiter time to reach the Condvar wait.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !waiter.is_finished(),
            "waiter must block on the in-flight install"
        );

        // Installer finishes successfully.
        rt.mark_installed(0);
        let outcome = waiter.join().unwrap();
        assert_eq!(
            outcome,
            Claim::Done,
            "a fault racing a successful install must wake, not re-install"
        );
    }

    /// Acceptance #3 + #1: same race, but the in-flight install FAILS and
    /// rolls back. The waiter must then re-claim and retry itself (so a
    /// transient producer error is repaired by the next fault, not left
    /// to hang).
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn concurrent_fault_during_failed_install_retries_itself() {
        let Some(rt) = bookkeeping_runtime(1) else {
            return;
        };
        assert_eq!(rt.claim_chunk(0), Claim::Claimed);

        let rt_w = Arc::clone(&rt);
        let waiter = std::thread::spawn(move || rt_w.claim_for_install(0));

        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !waiter.is_finished(),
            "waiter must block on the in-flight install"
        );

        // In-flight install fails -> rollback. The waiter, finding the
        // chunk Empty, must re-claim and own the retry.
        rt.release_chunk(0);
        let outcome = waiter.join().unwrap();
        assert_eq!(
            outcome,
            Claim::Claimed,
            "after a failed in-flight install the racing fault must retry, not wake a missing page"
        );
    }

    /// Acceptance #4: drain/seal accounting (`chunk_installed`, the basis
    /// of `sealed_uninstalled_count`) must be `Installed`-exact. A chunk
    /// merely `Installing`, or one whose install FAILED (back to
    /// `Empty`), must NOT count as installed — otherwise PeerLost
    /// under-reports `remaining` and the host-agent thinks doomed bytes
    /// are present.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_install_stays_uninstalled_for_accounting() {
        let Some(rt) = bookkeeping_runtime(1) else {
            return;
        };
        assert!(!rt.chunk_installed(0), "Empty is not installed");
        assert_eq!(rt.claim_chunk(0), Claim::Claimed);
        assert!(!rt.chunk_installed(0), "Installing is not installed");
        rt.release_chunk(0);
        assert!(
            !rt.chunk_installed(0),
            "a failed (rolled-back) install must not count as installed"
        );
        // Only a published success counts.
        assert_eq!(rt.claim_chunk(0), Claim::Claimed);
        rt.mark_installed(0);
        assert!(rt.chunk_installed(0));
    }

    /// Acceptance #1, end-to-end through a real UFFD: a `fetch_chunk`
    /// that FAILS on the first fault (chunk content absent from the
    /// store) must roll the chunk back so the SECOND fault — after the
    /// content lands — actually retries the fetch and installs the bytes.
    /// Before the fix the first fault set the bit, the error propagated,
    /// and every later fault returned `Ok(false)` → `wake_page` on a
    /// never-populated page → permanent fault/wake spin.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn fetch_failure_rolls_back_then_retries() {
        let page_size = 4096u64;
        let chunk_size = page_size;
        let n_chunks = 1usize;
        let total = chunk_size * n_chunks as u64;
        const TAG: u8 = 0x5A;

        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = ChunkStore::new(blob.clone());
        // Compute the chunk hash WITHOUT storing the content, so the
        // first fetch fails (blob-not-found) — the transient store error
        // #206 is about. We re-derive the hash by hashing in a throwaway
        // store, then build the manifest pointing the session at it.
        let content = vec![TAG; chunk_size as usize];
        let throwaway_dir = tempfile::tempdir().unwrap();
        let throwaway: Arc<dyn BlobStorage> =
            Arc::new(LocalBlobStorage::new(throwaway_dir.path().to_path_buf()));
        let hash = ChunkStore::new(throwaway)
            .put_chunk(&content)
            .await
            .unwrap();

        let manifest = Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Memory,
            chunk_size: ChunkSize::bytes(chunk_size),
            total_bytes: total,
            chunks: vec![ChunkRef { offset: 0, hash }],
            parent: None,
            annotations: serde_json::Value::Null,
        };
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        // Keep a handle to the (initially empty) backing store so the
        // test can land the content between the failing and retrying
        // faults, simulating the transient store error clearing.
        let store_handle = store.clone();
        let backend = Arc::new(
            ChunkedMemoryBackend::new(
                &manifest,
                &manifest,
                engram_chunk_store::reader::ChunkCacheReader::new(cfg.root.clone()),
                None,
                store,
            )
            .unwrap(),
        );

        let len = total as usize;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap failed");
        let uffd = match UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(false)
            .user_mode_only(true)
            .create()
        {
            Ok(u) => u,
            Err(e) => {
                unsafe { libc::munmap(ptr, len) };
                eprintln!("SKIP: cannot create userfaultfd ({e})");
                return;
            }
        };
        uffd.register(ptr, len).expect("register region with uffd");
        let mappings = vec![GuestRegionUffdMapping {
            base_host_virt_addr: ptr as u64,
            size: len,
            offset: 0,
            page_size: page_size as usize,
        }];
        let rt = Arc::new(
            Runtime::new(
                mappings,
                uffd,
                backend,
                tokio::runtime::Handle::current(),
                None,
            )
            .unwrap(),
        );

        let base = ptr as u64;
        // First fault: the content isn't in the store yet -> fetch fails
        // -> serve_pagefault returns Err and the chunk MUST roll back.
        let rt_e = Arc::clone(&rt);
        let first = std::thread::spawn(move || rt_e.serve_pagefault(base))
            .join()
            .unwrap();
        assert!(
            first.is_err(),
            "first fault should surface the transient fetch error"
        );
        assert!(
            !rt.chunk_installed(0),
            "a failed fetch must NOT leave the chunk marked installed (the #206 poison)"
        );
        assert_eq!(
            rt.claim_chunk(0),
            Claim::Claimed,
            "the chunk must be re-claimable after the failed fetch"
        );
        rt.release_chunk(0); // undo the probe claim

        // The content lands (transient error cleared).
        let stored = store_handle.put_chunk(&content).await.unwrap();
        assert_eq!(
            stored, hash,
            "stored content must hash to the manifest entry"
        );

        // Second fault: the resolver path is NOT gated on a stale bit, so
        // it retries the fetch and installs for real.
        let rt_ok = Arc::clone(&rt);
        std::thread::spawn(move || {
            rt_ok.serve_pagefault(base).expect("retry fault installs");
        })
        .join()
        .unwrap();
        assert!(rt.chunk_installed(0), "retry should install the chunk");

        let view = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        assert!(
            view.iter().all(|b| *b == TAG),
            "installed bytes must match the (now-available) chunk content"
        );
        unsafe { libc::munmap(ptr, len) };
    }
}
