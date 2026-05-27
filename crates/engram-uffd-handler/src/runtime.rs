//! UFFD event loop. Linux-only — `userfaultfd(2)` is a Linux kernel
//! syscall and the upstream `userfaultfd` crate fails to compile on
//! other targets.
//!
//! ADR 0007 chunked-memory shape (this revision):
//!
//! - The handler mmaps the **canonical** memory snapshot (one file
//!   per image, shared by every session of that image via the host
//!   page cache) read-only with `MAP_PRIVATE | MAP_POPULATE`.
//! - Per-fault, we consult a [`ChunkedMemoryBackend`] (see
//!   [`crate::chunked`]) to decide whether the page serves from the
//!   canonical mmap or from a session-divergent chunk.
//! - On every fault we install a **full 512 KiB chunk** in one
//!   `UFFDIO_COPY`, not just the faulting page. This amortises the
//!   syscall + fault-handler cost across the chunk's 128 pages and
//!   gets us cleanly under 100 ms for typical Python workloads
//!   after the working-set trace warms.
//! - On restore, if a [`WorkingSetTrace`] is supplied we prefault
//!   every chunk in the trace (REAP-style) **before** vCPUs run.
//! - As faults are served we record session-divergent chunk hashes
//!   into a [`WorkingSetRecorder`] so the next restore on this host
//!   can prefault them.
//!
//! `unsafe` blocks in this module are kernel-surface essentials
//! (mmap, UFFDIO_COPY, fd ownership from SCM_RIGHTS, `Send`
//! markers); each is annotated with a `// SAFETY:` comment.

#![cfg(target_os = "linux")]

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use engram_chunk_store::working_set::WorkingSetTrace;
use sendfd::RecvWithFd;
use tokio::runtime::Handle as TokioHandle;
use userfaultfd::{Event, Uffd};

use crate::chunked::{ChunkedBackendError, ChunkedMemoryBackend, ResolvedPage};
use crate::proto::{GuestRegionUffdMapping, HANDSHAKE_BUF_BYTES};
use crate::working_set::WorkingSetRecorder;

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
/// Used by both per-fault serving and prefault: every byte-offset
/// translation funnels through here.
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

/// All the state the event loop needs after the handshake.
pub struct Runtime {
    mappings: Vec<GuestRegionUffdMapping>,
    uffd: Uffd,
    /// Read-only mmap of the canonical memory file. `MAP_PRIVATE`
    /// means future writes (none in our path) would COW; in practice
    /// the kernel shares the underlying page-cache pages across every
    /// session of this image. `MAP_POPULATE` pre-faults the host
    /// pages so we don't take our own page faults while servicing
    /// the guest's.
    canonical_ptr: *const u8,
    canonical_size: usize,
    /// Hold the canonical file open so the kernel doesn't drop our
    /// mmap backing.
    _canonical_file: File,
    backend: Arc<ChunkedMemoryBackend>,
    /// Tokio handle used by the (sync) fault loop to drive
    /// async chunk fetches via `Handle::block_on`. The fault loop
    /// runs on a `spawn_blocking` thread which is safe to call
    /// `block_on` from.
    handle: TokioHandle,
    /// Working-set recorder shared with whoever publishes the trace
    /// on shutdown. `Mutex` is here for correctness, not contention:
    /// the fault loop is single-threaded today.
    recorder: Arc<Mutex<WorkingSetRecorder>>,
    /// Per-chunk installation tracking. Set to `true` once a chunk's
    /// bytes are in the guest. Lets the fault loop skip duplicate
    /// `UFFDIO_COPY` attempts for chunks already pre-faulted from
    /// a trace (`EEXIST` would otherwise force a retry).
    installed: Mutex<Vec<bool>>,
    page_size: u64,
    /// ADR 0014 M1.14: when set, the fault loop periodically writes
    /// the current recorder snapshot to this path. Belt-and-
    /// suspenders: the bake's profile pass can read the trace
    /// even if the run loop hangs on a stale userfaultfd that the
    /// kernel never closes after FC dies.
    trace_output: Option<PathBuf>,
}

// SAFETY: canonical_ptr is owned by this struct and only read; no
// mutable aliasing or threading concerns. We never expose `&mut`
// access to its bytes.
unsafe impl Send for Runtime {}
unsafe impl Sync for Runtime {}

impl Runtime {
    /// Open `canonical_memory`, mmap it `PROT_READ | MAP_PRIVATE |
    /// MAP_POPULATE`, then pair it with the chunked backend and the
    /// UFFD the handshake gave us.
    ///
    /// `recorder_window` controls how long the trace recorder stays
    /// open after construction. Pass `Duration::ZERO` to disable
    /// recording (useful for unit tests / one-shot replays).
    pub fn new(
        canonical_memory: &Path,
        mappings: Vec<GuestRegionUffdMapping>,
        uffd: Uffd,
        backend: Arc<ChunkedMemoryBackend>,
        handle: TokioHandle,
        recorder_window: Duration,
    ) -> Result<Self, HandlerError> {
        let file = File::open(canonical_memory).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("open canonical_memory {}: {e}", canonical_memory.display()),
            )
        })?;
        let size = file.metadata()?.len() as usize;
        if size as u64 != backend.total_bytes() {
            return Err(HandlerError::MappingSizeMismatch {
                mappings_total: size as u64,
                manifest_total: backend.total_bytes(),
            });
        }

        let mappings_total: u64 = mappings.iter().map(|m| m.size as u64).sum();
        if mappings_total != backend.total_bytes() {
            return Err(HandlerError::MappingSizeMismatch {
                mappings_total,
                manifest_total: backend.total_bytes(),
            });
        }

        // SAFETY: file is a regular file we just opened, fd is valid,
        // size matches its length. PROT_READ + MAP_PRIVATE means we
        // can't accidentally write to the snapshot. MAP_POPULATE
        // pre-faults the pages in our own address space so when we
        // dereference `canonical_ptr` to fill a guest fault we don't
        // block on a host-side page fault.
        let raw = {
            // ADR 0019: MAP_POPULATE synchronously faults the *entire*
            // canonical memory file into our address space right here — a
            // restore critical-path cost (H2: candidate to drop in favour of
            // lazy fault + working-set prefetch). Span it so a restore trace
            // shows the populate time distinct from the rest of restore.
            let _span = tracing::info_span!("uffd.map_populate", bytes = size as u64).entered();
            unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    size,
                    libc::PROT_READ,
                    libc::MAP_PRIVATE | libc::MAP_POPULATE,
                    file.as_raw_fd(),
                    0,
                )
            }
        };
        if raw == libc::MAP_FAILED {
            return Err(HandlerError::Io(std::io::Error::last_os_error()));
        }

        // `page_size` is whatever the registered region reported.
        // FC carves the guest's address space into 4 KiB pages on
        // x86 and 16 KiB on aarch64; per-region the kernel guarantees
        // this is consistent.
        let page_size = mappings.first().map(|m| m.page_size as u64).unwrap_or(4096);
        let chunk_count = backend.total_bytes().div_ceil(backend.chunk_size()) as usize;
        let vcpu_count = u32::try_from(mappings.len()).unwrap_or(u32::MAX);
        let recorder = Arc::new(Mutex::new(WorkingSetRecorder::new(
            vcpu_count,
            recorder_window,
        )));

        Ok(Self {
            mappings,
            uffd,
            canonical_ptr: raw as *const u8,
            canonical_size: size,
            _canonical_file: file,
            backend,
            handle,
            recorder,
            installed: Mutex::new(vec![false; chunk_count]),
            page_size,
            trace_output: None,
        })
    }

    /// ADR 0014 M1.14: configure periodic trace_output dumping. The
    /// fault loop snapshots the recorder into the path every N
    /// faults (and once after the recorder window closes) so the
    /// trace lands on disk even if the loop doesn't unwind cleanly.
    pub fn set_trace_output(&mut self, path: PathBuf) {
        self.trace_output = Some(path);
    }

    /// Snapshot the current recorder state and write it to
    /// `trace_output` if configured. Best-effort; failures are
    /// logged but don't propagate (the path matters more than
    /// our error reporting).
    fn dump_trace_output(&self) {
        let Some(path) = self.trace_output.as_ref() else {
            return;
        };
        let r = self.recorder.lock().expect("recorder poisoned");
        let mut trace = WorkingSetTrace::new(r.vcpu_count_snapshot(), r.window_ms_snapshot());
        trace.chunks = r.chunks_snapshot();
        drop(r);
        match serde_json::to_vec(&trace) {
            Ok(bytes) => {
                let tmp = path.with_extension("json.tmp");
                if let Err(e) = std::fs::write(&tmp, &bytes) {
                    tracing::warn!(error = %e, path = %tmp.display(), "trace_output tmp write");
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp, path) {
                    tracing::warn!(error = %e, path = %path.display(), "trace_output rename");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "trace_output serialize");
            }
        }
    }

    /// Pre-install every chunk listed in `trace` into the guest
    /// before vCPUs run. Idempotent against partial overlap with a
    /// later fault-served install (per-chunk bitmap guards the
    /// double-install case).
    ///
    /// Drops the trace lock between chunks so a follow-up
    /// pre-fault from a fresh trace can interleave with the fault
    /// loop — useful only in testing; production always pre-faults
    /// once at startup.
    pub fn prefault_from_trace(&self, trace: &WorkingSetTrace) -> Result<(), HandlerError> {
        let mut installed = 0usize;
        let mut skipped = 0usize;
        for hash in &trace.chunks {
            // Fetch once per hash; install at every position the
            // session manifest places the chunk at.
            let positions = self.backend.session_positions_of(*hash);
            if positions.is_empty() {
                // Trace mentions a hash the session no longer needs
                // (rewritten / GC'd). Cheap to skip.
                skipped += 1;
                continue;
            }
            let bytes = self.handle.block_on(self.backend.fetch_chunk(*hash))?;
            for byte_offset in positions {
                if self.install_chunk_at(byte_offset, bytes.clone(), false)? {
                    installed += 1;
                }
            }
        }
        tracing::info!(
            installed,
            skipped,
            trace_len = trace.chunks.len(),
            "prefault from working-set trace complete"
        );
        Ok(())
    }

    /// Install `bytes` (a full chunk) at the given session byte
    /// offset, copying into the guest's UFFD-registered region via
    /// one `UFFDIO_COPY`. `wake` controls whether vCPUs blocked on
    /// pages in this range are woken (`true` during the fault loop,
    /// `false` during prefault when no vCPU is blocked yet).
    ///
    /// Returns `Ok(true)` if we installed; `Ok(false)` if the chunk
    /// was already installed (idempotent path).
    fn install_chunk_at(
        &self,
        byte_offset: u64,
        bytes: Bytes,
        wake: bool,
    ) -> Result<bool, HandlerError> {
        let chunk_size = self.backend.chunk_size();
        let chunk_idx = (byte_offset / chunk_size) as usize;

        // First, the cheap idempotency check — avoid syscalls and
        // EEXIST round-trips entirely if a prior install already
        // covered this chunk.
        {
            let mut installed = self.installed.lock().expect("installed bitmap poisoned");
            if let Some(slot) = installed.get_mut(chunk_idx) {
                if *slot {
                    return Ok(false);
                }
                *slot = true;
            }
        }

        let (_m, host_va, room_to_eom) = locate_offset(&self.mappings, byte_offset)
            .ok_or(HandlerError::AddressOutsideRegions(byte_offset))?;

        // Install the smaller of (chunk_size, remaining mapping size,
        // bytes.len()). Cross-region chunks are unusual but tolerated
        // by clamping to the current region's tail; the next
        // (mis-aligned) region's fault will install its own chunk.
        let install_len =
            std::cmp::min(std::cmp::min(chunk_size, room_to_eom), bytes.len() as u64) as usize;
        debug_assert!(install_len.is_multiple_of(self.page_size as usize));

        // SAFETY: `bytes.as_ptr()` is valid for `install_len` bytes
        // (we just clamped). `host_va` is a guest-visible address
        // in a UFFD-registered region we own a copy access to via
        // the kernel. UFFDIO_COPY semantics handle the page table
        // entry install + wake atomically per page.
        unsafe {
            self.uffd.copy(
                bytes.as_ptr() as *const _,
                host_va as *mut std::ffi::c_void,
                install_len,
                wake,
            )?;
        }
        Ok(true)
    }

    /// Install a canonical-resolved range: copy from our own
    /// canonical mmap into the guest. Same semantics as
    /// `install_chunk_at` but the source is `canonical_ptr +
    /// canonical_offset` instead of a fetched `Bytes`.
    fn install_canonical_at(
        &self,
        canonical_offset: u64,
        byte_offset: u64,
        wake: bool,
    ) -> Result<bool, HandlerError> {
        let chunk_size = self.backend.chunk_size();
        let chunk_idx = (byte_offset / chunk_size) as usize;

        {
            let mut installed = self.installed.lock().expect("installed bitmap poisoned");
            if let Some(slot) = installed.get_mut(chunk_idx) {
                if *slot {
                    return Ok(false);
                }
                *slot = true;
            }
        }

        let (_m, host_va, room_to_eom) = locate_offset(&self.mappings, byte_offset)
            .ok_or(HandlerError::AddressOutsideRegions(byte_offset))?;

        let canonical_end = canonical_offset + chunk_size;
        let canonical_tail = if canonical_end > self.canonical_size as u64 {
            self.canonical_size as u64 - canonical_offset
        } else {
            chunk_size
        };
        let install_len = std::cmp::min(canonical_tail, room_to_eom) as usize;
        debug_assert!(install_len.is_multiple_of(self.page_size as usize));
        debug_assert!(canonical_offset as usize + install_len <= self.canonical_size);

        // SAFETY: `canonical_ptr + canonical_offset` is inside our
        // mmap (bounds checked above). `host_va` is the guest range
        // we have UFFD write access to.
        unsafe {
            let src = self.canonical_ptr.add(canonical_offset as usize);
            self.uffd.copy(
                src as *const _,
                host_va as *mut std::ffi::c_void,
                install_len,
                wake,
            )?;
        }
        Ok(true)
    }

    /// Run forever, draining events from the UFFD and serving each
    /// page fault. Returns `Ok(())` cleanly when the UFFD is closed
    /// (Firecracker exited / sandbox destroyed).
    pub fn run(&self) -> Result<(), HandlerError> {
        let mut faults_served: u64 = 0;
        loop {
            match self.uffd.read_event() {
                Ok(Some(Event::Pagefault { addr, .. })) => {
                    self.serve_pagefault(addr as u64)?;
                    faults_served += 1;
                    if faults_served.is_power_of_two() {
                        tracing::info!(faults_served, "served page fault");
                        // ADR 0014 M1.14: dump trace_output on the
                        // same log-cadence so a hung run loop still
                        // produces a usable file. Cheap (~1 KiB
                        // serialize + fs::write).
                        self.dump_trace_output();
                    }
                }
                Ok(Some(other)) => {
                    tracing::debug!(?other, "non-pagefault uffd event");
                }
                Ok(None) => {
                    return Ok(());
                }
                Err(userfaultfd::Error::ReadEof) => {
                    tracing::info!(
                        faults_served,
                        "uffd closed by firecracker; handler exiting cleanly"
                    );
                    return Ok(());
                }
                Err(e) => return Err(HandlerError::Uffd(e)),
            }
        }
    }

    fn serve_pagefault(&self, fault_addr: u64) -> Result<(), HandlerError> {
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

        match self
            .backend
            .resolve(chunk_byte_offset)
            .ok_or(HandlerError::AddressOutsideRegions(fault_addr))?
        {
            ResolvedPage::Canonical { canonical_offset } => {
                // ADR 0014 M1.14: also record the canonical chunk
                // hash. Without this, a fully-canonical snapshot
                // (bake time, pre-divergence) produces an empty
                // working-set trace and the warm-pool refill prefetch
                // can't narrow at all.
                if let Some(hash) = self.backend.canonical_chunk_hash(canonical_offset) {
                    let mut r = self.recorder.lock().expect("recorder poisoned");
                    if r.is_open() {
                        r.observe(hash);
                    }
                }
                let installed =
                    self.install_canonical_at(canonical_offset, chunk_byte_offset, true)?;
                if !installed {
                    // Already installed: wake the vCPU since the
                    // kernel may have queued the fault before our
                    // earlier prefault landed.
                    self.wake_page(page_aligned, page_size)?;
                }
            }
            ResolvedPage::Chunk { hash } => {
                // Track in the working-set recorder regardless of
                // whether the install fires — the hash is what the
                // *next* restore wants to prefault.
                {
                    let mut r = self.recorder.lock().expect("recorder poisoned");
                    if r.is_open() {
                        r.observe(hash);
                    }
                }
                let bytes = self.handle.block_on(self.backend.fetch_chunk(hash))?;
                let installed = self.install_chunk_at(chunk_byte_offset, bytes, true)?;
                if !installed {
                    self.wake_page(page_aligned, page_size)?;
                }
            }
        }

        Ok(())
    }

    /// `UFFDIO_WAKE` the specified page. Used when the chunk this
    /// fault belongs to was already installed (by an earlier
    /// prefault or sibling-fault install): the page is no longer
    /// missing but the vCPU was still queued, so we have to tell
    /// the kernel explicitly.
    fn wake_page(&self, page_aligned: u64, page_size: u64) -> Result<(), HandlerError> {
        self.uffd
            .wake(page_aligned as *mut std::ffi::c_void, page_size as usize)?;
        Ok(())
    }

    /// Consume the runtime, freezing the working-set recorder into a
    /// publishable trace. Caller uploads it to
    /// `traces/<manifest_id>/<host_id>.json` for the next restore.
    pub fn finish_recorder(&self) -> WorkingSetTrace {
        let recorder = std::mem::replace(
            &mut *self.recorder.lock().expect("recorder poisoned"),
            WorkingSetRecorder::new(0, Duration::ZERO),
        );
        recorder.finish()
    }

    /// Test/debug accessor.
    #[allow(dead_code)]
    pub fn mappings(&self) -> &[GuestRegionUffdMapping] {
        &self.mappings
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: canonical_ptr was returned by libc::mmap on
        // creation and we hold the only reference to it; munmap
        // with the same size is the matching cleanup.
        unsafe {
            libc::munmap(self.canonical_ptr as *mut _, self.canonical_size);
        }
    }
}

/// One-shot run: bind the listener, accept Firecracker, handshake,
/// optionally pre-fault from a trace, then loop until the UFFD
/// closes. On clean exit, returns the recorded working-set trace
/// (which the caller publishes back to the chunk store).
///
/// `stream` is held alive for the lifetime of the run — matches the
/// upstream `on_demand_handler.rs` which polls both the stream and
/// the UFFD. Dropping the stream after the handshake caused
/// Firecracker to hang on `PUT /snapshot/load` (it keeps its side
/// open and treats our close as a protocol error).
pub fn run_listener(
    listen: PathBuf,
    canonical_memory: PathBuf,
    backend: Arc<ChunkedMemoryBackend>,
    handle: TokioHandle,
    prefault_trace: Option<WorkingSetTrace>,
    recorder_window: Duration,
    trace_output: Option<PathBuf>,
) -> Result<WorkingSetTrace, HandlerError> {
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
    let mut rt = Runtime::new(
        &canonical_memory,
        mappings,
        uffd,
        backend,
        handle,
        recorder_window,
    )?;
    if let Some(path) = trace_output {
        rt.set_trace_output(path);
    }
    if let Some(trace) = prefault_trace {
        rt.prefault_from_trace(&trace)?;
    }
    let _stream_alive = stream;
    tracing::info!(pid, "starting fault loop");
    let result = rt.run();
    tracing::info!(pid, ?result, "fault loop returned");
    result?;
    Ok(rt.finish_recorder())
}
