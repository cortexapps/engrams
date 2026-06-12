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
//!   100 ms for typical Python workloads after the working-set trace
//!   warms.
//! - On restore, if a [`WorkingSetTrace`] is supplied we prefault
//!   every chunk in the trace (REAP-style) **before** vCPUs run.
//! - As faults are served we record observed chunk hashes into a
//!   [`WorkingSetRecorder`] so the next restore on this host can
//!   prefault them.
//!
//! Dropping the canonical mmap is what removes the `materialize`
//! pass + the `MAP_POPULATE` eager read from the restore critical
//! path (ADR 0020). Cross-session *guest-RAM* dedup is **not**
//! provided by this revision (`UFFDIO_COPY` always installs a private
//! page); the future packing milestone layers `UFFDIO_CONTINUE` over
//! a shared per-image backing on top of this same resolution logic.
//!
//! `unsafe` blocks in this module are kernel-surface essentials
//! (UFFDIO_COPY / UFFDIO_ZEROPAGE, fd ownership from SCM_RIGHTS);
//! each is annotated with a `// SAFETY:` comment.

#![cfg(target_os = "linux")]

use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
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
    /// ADR 0045 unified memory substrate (v2b). When set, guest memory
    /// is `MAP_PRIVATE` of this per-template base shm file (registered
    /// `MISSING|MINOR` by the forked FC) and canonical chunks install
    /// via populate-the-base + `UFFDIO_CONTINUE` — one page-cache copy
    /// shared by every same-template VM on the host — instead of a
    /// private `UFFDIO_COPY`. Session-divergent chunks keep COPY.
    /// `None` ⇒ stock Route-B behavior, byte-identical.
    base_shm: Option<crate::base_shm::BaseShm>,
    /// Whether `UFFDIO_ZEROPAGE` works on the substrate's MAP_PRIVATE
    /// file-backed mapping (kernel-version dependent). Starts `true`;
    /// flips to `false` on the first EINVAL and zero chunks fall back
    /// to a private COPY of a zeroed buffer.
    zeropage_ok: std::sync::atomic::AtomicBool,
    /// ADR 0045 C2: post-copy migration mode. Sealed (dirtied-since-
    /// checkpoint) chunks are peer-authoritative: the fault path asks
    /// the source's page server instead of `resolve()`, and the drain
    /// task pulls the rest in the background. `None` ⇒ everything
    /// above is byte-identical to a C1 restore.
    peer: Option<std::sync::Arc<crate::peer::PeerSession>>,
    /// One-way progress/failure reports to the host-agent (peer mode).
    control: Option<std::sync::Arc<crate::peer::ControlTx>>,
    /// ADR 0045 C2 (E2B fold): one-shot trace dump when the recorder
    /// window closes — the migration capture reads the per-jail trace
    /// file LIVE, so waiting for handler exit (the publish channel) is
    /// too late. Set once the close-dump has fired.
    window_dump_done: std::sync::atomic::AtomicBool,
}

impl Runtime {
    /// Pair the chunked backend with the UFFD the handshake gave us.
    /// No `memory.bin` file is opened — every page is served from
    /// chunks (ADR 0020 Route B).
    ///
    /// `recorder_window` controls how long the trace recorder stays
    /// open after construction. Pass `Duration::ZERO` to disable
    /// recording (useful for unit tests / one-shot replays).
    pub fn new(
        mappings: Vec<GuestRegionUffdMapping>,
        uffd: Uffd,
        backend: Arc<ChunkedMemoryBackend>,
        handle: TokioHandle,
        recorder_window: Duration,
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
        let chunk_count = backend.total_bytes().div_ceil(backend.chunk_size()) as usize;
        let vcpu_count = u32::try_from(mappings.len()).unwrap_or(u32::MAX);
        let recorder = Arc::new(Mutex::new(WorkingSetRecorder::new(
            vcpu_count,
            recorder_window,
        )));

        Ok(Self {
            mappings,
            uffd,
            backend,
            handle,
            recorder,
            installed: Mutex::new(vec![false; chunk_count]),
            page_size,
            trace_output: None,
            base_shm,
            zeropage_ok: std::sync::atomic::AtomicBool::new(true),
            peer: None,
            control: None,
            window_dump_done: std::sync::atomic::AtomicBool::new(false),
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
        self.peer = Some(peer);
        self.control = control;
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
            // ADR 0045 substrate: canonical positions must install via the
            // shared base + CONTINUE — a prefault COPY here would privately
            // duplicate exactly the hot pages the substrate exists to share.
            if let Some(base) = self.base_shm.as_ref() {
                for byte_offset in positions {
                    let shared = matches!(
                        self.backend.resolve(byte_offset),
                        Some(ResolvedPage::Canonical { canonical_offset })
                            if self.backend.canonical_chunk_hash(canonical_offset)
                                == Some(*hash)
                    );
                    let did = if shared {
                        self.install_canonical_shared(base, byte_offset, *hash, false)?
                    } else {
                        let bytes = self.handle.block_on(self.backend.fetch_chunk(*hash))?;
                        self.install_chunk_at(byte_offset, bytes, false)?
                    };
                    if did {
                        installed += 1;
                    }
                }
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

    /// ADR 0045 C1 tail latency: eagerly install EVERY chunk of the
    /// session — canonical via the shared base (CONTINUE), divergent
    /// via fetch + COPY — as a background producer racing the fault
    /// loop, exactly like `prefault_from_trace` but with total
    /// coverage instead of a recorded working set. A migration restore
    /// has no host trace, so before this sweep its divergent chunks
    /// (the checkpoint chain's accumulated diff — hundreds of 512 KiB
    /// chunks, all prestaged on local NVMe) faulted in one at a time
    /// through the single-threaded fault loop: the residual ~27 s
    /// post-teleport guest crawl (prod canary 51d51740). Idempotent
    /// against the fault loop via the `installed` bitmap; `wake=false`
    /// (no vCPU waits on a page it hasn't faulted).
    pub fn sweep_all(&self) -> Result<(), HandlerError> {
        if std::env::var("ENGRAM_UFFD_EAGER_SWEEP")
            .map(|v| v == "0")
            .unwrap_or(false)
        {
            tracing::info!("eager sweep disabled via ENGRAM_UFFD_EAGER_SWEEP=0");
            return Ok(());
        }
        // Time-box: each per-chunk install completes atomically (the
        // `installed` bit and the bytes land together), so stopping
        // BETWEEN chunks is always safe — the rest serve on-demand. A
        // stuck origin fetch inside one chunk is the dangerous case
        // (its bit is set; a faulting vCPU would wake onto a missing
        // page), which is why fetches go through the cache's bounded
        // path — the budget here is the backstop that keeps a slow
        // sweep from monopolizing I/O long past its usefulness.
        let budget = std::time::Duration::from_secs(
            std::env::var("ENGRAM_UFFD_EAGER_SWEEP_BUDGET_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
        );
        let started = std::time::Instant::now();
        let chunk_size = self.backend.chunk_size();
        let total = self.backend.total_bytes();
        let mut installed = 0usize;
        let mut deferred = 0usize;
        // Two passes, divergent first: those are the chunks an
        // on-demand fault pays a fetch for (the post-teleport crawl);
        // canonical pages CONTINUE from the (pre-warmed) base for
        // near-free, and ZERO chunks are skipped entirely — eagerly
        // installing them would privately materialize every untouched
        // page of guest RAM (UFFDIO_ZEROPAGE is rejected on the
        // substrate's MAP_PRIVATE file mapping, so each would COPY a
        // zeroed buffer), defeating the density the substrate exists
        // for. A zero fault is already served with no fetch.
        for pass in ["divergent", "canonical"] {
            let mut offset: u64 = 0;
            while offset < total {
                if started.elapsed() > budget {
                    tracing::warn!(
                        pass,
                        installed,
                        deferred,
                        "eager sweep budget exhausted; remaining chunks serve on-demand"
                    );
                    return Ok(());
                }
                // ADR 0045 C2: a sealed-but-undrained chunk's truth is the
                // PEER, not the manifest — installing resolve()'s stale
                // view here would be silent corruption. The drain runs
                // before this on the producer thread, so normally every
                // sealed chunk is already installed; this guard is the
                // belt-and-braces for any other interleaving.
                if let Some(peer) = self.peer.as_ref() {
                    let idx = offset / chunk_size;
                    if peer.seal().get(idx) && !self.chunk_installed(idx as usize) {
                        offset += chunk_size;
                        continue;
                    }
                }
                let did = match (pass, self.backend.resolve(offset)) {
                    ("divergent", Some(ResolvedPage::Chunk { hash })) => {
                        let bytes = self.handle.block_on(self.backend.fetch_chunk(hash))?;
                        self.install_chunk_at(offset, bytes, false)?
                    }
                    ("canonical", Some(ResolvedPage::Canonical { canonical_offset })) => {
                        match self.backend.canonical_chunk_hash(canonical_offset) {
                            Some(hash) => match self.base_shm.as_ref() {
                                Some(base) => {
                                    self.install_canonical_shared(base, offset, hash, false)?
                                }
                                None => {
                                    let bytes =
                                        self.handle.block_on(self.backend.fetch_chunk(hash))?;
                                    self.install_chunk_at(offset, bytes, false)?
                                }
                            },
                            None => {
                                deferred += 1;
                                false
                            }
                        }
                    }
                    _ => false,
                };
                if did {
                    installed += 1;
                }
                offset += chunk_size;
            }
        }
        tracing::info!(
            installed,
            zero_skipped = deferred,
            elapsed_ms = started.elapsed().as_millis() as u64,
            total_chunks = total.div_ceil(chunk_size),
            "eager sweep complete (ADR 0045 C1)"
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

    /// Install a zero-filled chunk: `UFFDIO_ZEROPAGE` over the
    /// chunk-aligned range. Used for canonical chunks the manifest
    /// omits (implicit zero pages). No copy — the kernel maps its
    /// shared zero page, COW on the guest's first write. Same
    /// idempotency + clamping discipline as `install_chunk_at`.
    fn install_zero_at(&self, byte_offset: u64, wake: bool) -> Result<bool, HandlerError> {
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
        let install_len = std::cmp::min(chunk_size, room_to_eom) as usize;
        debug_assert!(install_len.is_multiple_of(self.page_size as usize));

        // SAFETY: `host_va` is a guest-visible address in a UFFD-
        // registered region we hold copy/zeropage access to. ZEROPAGE
        // installs zero pages over the range + wakes blocked vCPUs
        // atomically per page when `wake`.
        unsafe {
            self.uffd
                .zeropage(host_va as *mut std::ffi::c_void, install_len, wake)?;
        }
        Ok(true)
    }

    /// ADR 0045 substrate (v2b): install a canonical chunk by ensuring the
    /// base shm file holds its bytes, then `UFFDIO_CONTINUE`-ing the range —
    /// the kernel maps the base file's page-cache pages (write-protected
    /// MAP_PRIVATE; guest writes COW), so every same-template VM on the host
    /// shares one physical copy. The fetch is skipped entirely when a
    /// sibling already populated the range (`SEEK_HOLE` probe).
    fn install_canonical_shared(
        &self,
        base: &crate::base_shm::BaseShm,
        byte_offset: u64,
        hash: engram_chunk_store::ChunkHash,
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
        let populate_len = std::cmp::min(
            chunk_size,
            self.backend.total_bytes().saturating_sub(byte_offset),
        );
        if !base.is_populated(byte_offset, populate_len) {
            let bytes = self.handle.block_on(self.backend.fetch_chunk(hash))?;
            base.write_chunk(byte_offset, &bytes)
                .map_err(HandlerError::BaseShm)?;
        }

        let install_len =
            std::cmp::min(std::cmp::min(chunk_size, room_to_eom), populate_len) as usize;
        debug_assert!(install_len.is_multiple_of(self.page_size as usize));
        // CONTINUE the whole range in one ioctl; the kernel may map a prefix
        // and return EAGAIN-with-progress (surfaced by the crate as
        // Ok(mapped < len)) — loop the remainder. EEXIST means a racing
        // prefault/fault already mapped a page; treat as installed.
        let mut done: u64 = 0;
        while (done as usize) < install_len {
            let start = host_va + done;
            let len = install_len as u64 - done;
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
        Ok(true)
    }

    /// ADR 0045 substrate (v2b): zero canonical chunks. Try
    /// `UFFDIO_ZEROPAGE` (maps the kernel zero page — zero RAM until the
    /// guest writes); some kernels reject it on MAP_PRIVATE file-backed
    /// mappings, in which case fall back to a private COPY of a zeroed
    /// buffer (correct, costs one private page per touched page).
    fn install_zero_substrate(&self, byte_offset: u64, wake: bool) -> Result<bool, HandlerError> {
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
        let install_len = std::cmp::min(chunk_size, room_to_eom) as usize;

        use std::sync::atomic::Ordering;
        if self.zeropage_ok.load(Ordering::Relaxed) {
            // SAFETY: range is inside a registered region (as above).
            match unsafe {
                self.uffd
                    .zeropage(host_va as *mut std::ffi::c_void, install_len, wake)
            } {
                Ok(_) => return Ok(true),
                Err(userfaultfd::Error::ZeropageFailed(errno))
                    if errno as i32 == libc::EINVAL || errno as i32 == libc::EOPNOTSUPP =>
                {
                    self.zeropage_ok.store(false, Ordering::Relaxed);
                    tracing::info!(
                        "UFFDIO_ZEROPAGE unsupported on the substrate mapping;                          falling back to COPY-of-zeros"
                    );
                }
                Err(e) => return Err(HandlerError::Uffd(e)),
            }
        }
        let zeros = bytes::Bytes::from(vec![0u8; install_len]);
        // SAFETY: zeroed buffer of install_len; same contract as
        // install_chunk_at's copy.
        unsafe {
            self.uffd.copy(
                zeros.as_ptr() as *const _,
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
                    // ADR 0045 C2 (E2B fold): one-shot dump on the first
                    // fault AFTER the recorder window closes, so the
                    // complete hot set is on disk ~window-length after
                    // restore — a live migration capture reads it from
                    // the jail. (The power-of-two cadence alone can
                    // leave the file stale mid-window.)
                    if !self
                        .window_dump_done
                        .load(std::sync::atomic::Ordering::Relaxed)
                        && !self.recorder.lock().expect("recorder poisoned").is_open()
                        && !self
                            .window_dump_done
                            .swap(true, std::sync::atomic::Ordering::Relaxed)
                    {
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

        // ADR 0045 C2: sealed chunks are peer-authoritative — their truth
        // lives only in the paused source's address space, so they MUST
        // resolve via the peer, never via `resolve()` (whose manifest view
        // predates the dirtying). Ahead of everything else by design.
        if let Some(peer) = self.peer.as_ref() {
            let chunk_idx = (chunk_byte_offset / chunk_size) as usize;
            if peer.seal().get(chunk_idx as u64) {
                if self.chunk_installed(chunk_idx) {
                    // Drain (or an earlier fault) already landed it; the
                    // vCPU queued before the install — wake it.
                    self.wake_page(page_aligned, page_size)?;
                    return Ok(());
                }
                match peer.need_at(chunk_byte_offset) {
                    Ok(crate::peer::PeerPage::Bytes(bytes)) => {
                        let installed =
                            self.install_chunk_at(chunk_byte_offset, bytes.into(), true)?;
                        if !installed {
                            self.wake_page(page_aligned, page_size)?;
                        }
                        return Ok(());
                    }
                    Ok(crate::peer::PeerPage::Zero) => {
                        // Private zero install — NEVER the shared base
                        // (sealed content is divergence by definition).
                        let installed = if self.base_shm.is_some() {
                            self.install_zero_substrate(chunk_byte_offset, true)?
                        } else {
                            self.install_zero_at(chunk_byte_offset, true)?
                        };
                        if !installed {
                            self.wake_page(page_aligned, page_size)?;
                        }
                        return Ok(());
                    }
                    Ok(crate::peer::PeerPage::AltSource(_durable)) => {
                        // Over-approximation demote: the source proved this
                        // chunk equals the durable manifest entry, so the
                        // normal resolve() arms below serve it (class 2).
                    }
                    Err(e) => {
                        peer.mark_lost();
                        if let Some(control) = self.control.as_ref() {
                            control.report(engram_migrate_proto::HandlerControl::PeerLost {
                                remaining: self.sealed_uninstalled_count(peer),
                                detail: e.to_string(),
                            });
                        }
                        return Err(HandlerError::PeerLost(e.to_string()));
                    }
                }
            }
        }

        match self
            .backend
            .resolve(chunk_byte_offset)
            .ok_or(HandlerError::AddressOutsideRegions(fault_addr))?
        {
            ResolvedPage::Canonical { canonical_offset } => {
                // Route B: "canonical" no longer means "in the mmap" —
                // it means "fetch the canonical chunk hash from the
                // store", or, when the manifest omits this offset, a
                // zero-filled chunk we install via UFFDIO_ZEROPAGE.
                match self.backend.canonical_chunk_hash(canonical_offset) {
                    Some(hash) => {
                        // ADR 0014 M1.14: record the canonical chunk
                        // hash. Without this, a fully-canonical snapshot
                        // (base snapshot, pre-divergence) produces an
                        // empty working-set trace and the next restore's
                        // prefetch can't narrow at all.
                        {
                            let mut r = self.recorder.lock().expect("recorder poisoned");
                            if r.is_open() {
                                r.observe(hash);
                            }
                        }
                        let installed = match self.base_shm.as_ref() {
                            // ADR 0045 substrate: shared install via the
                            // base shm + CONTINUE (one physical copy per
                            // host); guest writes COW privately.
                            Some(base) => {
                                self.install_canonical_shared(base, chunk_byte_offset, hash, true)?
                            }
                            None => {
                                let bytes = self.handle.block_on(self.backend.fetch_chunk(hash))?;
                                self.install_chunk_at(chunk_byte_offset, bytes, true)?
                            }
                        };
                        if !installed {
                            // Already installed: wake the vCPU since the
                            // kernel may have queued the fault before our
                            // earlier prefault landed.
                            self.wake_page(page_aligned, page_size)?;
                        }
                    }
                    None => {
                        let installed = if self.base_shm.is_some() {
                            self.install_zero_substrate(chunk_byte_offset, true)?
                        } else {
                            self.install_zero_at(chunk_byte_offset, true)?
                        };
                        if !installed {
                            self.wake_page(page_aligned, page_size)?;
                        }
                    }
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

    /// Cheap read of the per-chunk install bit.
    fn chunk_installed(&self, chunk_idx: usize) -> bool {
        self.installed
            .lock()
            .expect("installed bitmap poisoned")
            .get(chunk_idx)
            .copied()
            .unwrap_or(false)
    }

    /// How many sealed chunks remain uninstalled (PeerLost reporting).
    fn sealed_uninstalled_count(&self, peer: &crate::peer::PeerSession) -> u64 {
        let installed = self.installed.lock().expect("installed bitmap poisoned");
        (0..peer.seal().chunk_count)
            .filter(|i| {
                peer.seal().get(*i) && !installed.get(*i as usize).copied().unwrap_or(false)
            })
            .count() as u64
    }

    /// ADR 0045 C2: the background drain — pull every sealed chunk the
    /// fault path hasn't already served, over the drain's OWN
    /// connection, pipelined. Runs FIRST on the producer thread (before
    /// the trace prefault and the eager sweep): sealed content exists
    /// only in the paused source, so draining it is what releases the
    /// source — and it makes the later sweep/prefault trivially safe
    /// (by the time they run, every sealed chunk is installed, so their
    /// stale `resolve()` view can never be installed over divergence).
    ///
    /// `AltSource` responses demote inline to the class-2 path (fetch
    /// via cache/store + private install). Install errors are real
    /// errors; peer errors return `Err` and the caller reports
    /// `PeerLost`.
    pub fn drain_from_peer(
        &self,
        peer: &crate::peer::PeerSession,
    ) -> Result<crate::peer::DrainStats, HandlerError> {
        use crate::peer::{DrainStats, PeerPage};

        const PIPELINE_DEPTH: usize = 8;

        let chunk_size = self.backend.chunk_size();
        let seal = peer.seal();
        let mut stats = DrainStats::default();
        let mut conn = peer
            .open_extra_conn()
            .map_err(|e| HandlerError::PeerLost(format!("drain conn: {e}")))?;
        let next_req = std::sync::atomic::AtomicU64::new(1_000_000); // distinct from fault-conn ids in logs

        // Pending sealed chunk offsets, skipping anything already
        // installed by a racing fault.
        let todo: Vec<u64> = (0..seal.chunk_count)
            .filter(|i| seal.get(*i))
            .map(|i| i * chunk_size)
            .collect();
        let total_sealed = todo.len();

        let mut in_flight: std::collections::VecDeque<(u64, u64)> = Default::default(); // (req_id, offset)
        let mut iter = todo.into_iter();
        let mut processed = 0usize;
        loop {
            // Fill the pipeline — but YIELD to guest faults. A fault
            // request queuing behind the drain's in-flight bulk bytes
            // was the measured ~7 ms/fault (vs ~1 ms on a quiet wire);
            // during a post-resume fault storm the guest's stall
            // matters and the drain's finish time does not (it only
            // delays the source release). Parking REFILLS only: the
            // up-to-8 outstanding responses flush in a few ms, then
            // the wire belongs to the fault path until it goes quiet.
            while peer.fault_active_within(std::time::Duration::from_millis(2)) {
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
            while in_flight.len() < PIPELINE_DEPTH {
                let Some(offset) = iter.next() else { break };
                if self.chunk_installed((offset / chunk_size) as usize) {
                    processed += 1;
                    continue;
                }
                let req_id = next_req.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                engram_migrate_proto::write_frame(
                    &mut conn,
                    &engram_migrate_proto::ToSource::NeedAt {
                        req_id,
                        chunk_offset: offset,
                    },
                )
                .map_err(|e| HandlerError::PeerLost(format!("drain write: {e}")))?;
                in_flight.push_back((req_id, offset));
            }
            let Some((req_id, offset)) = in_flight.pop_front() else {
                break; // pipeline empty and iterator exhausted
            };
            let resp = engram_migrate_proto::read_frame(&mut conn)
                .map_err(|e| HandlerError::PeerLost(format!("drain read: {e}")))?;
            let page = crate::peer::decode_page(resp, req_id, offset)
                .map_err(|e| HandlerError::PeerLost(format!("drain decode: {e}")))?;
            match page {
                PeerPage::Bytes(bytes) => {
                    self.install_chunk_at(offset, bytes.into(), false)?;
                    stats.pulled += 1;
                }
                PeerPage::Zero => {
                    if self.base_shm.is_some() {
                        self.install_zero_substrate(offset, false)?;
                    } else {
                        self.install_zero_at(offset, false)?;
                    }
                    stats.zero_chunks += 1;
                }
                PeerPage::AltSource(_durable) => {
                    // Class-2 demote: serve through the normal resolve
                    // arms (private install; the shared base is never
                    // written with session-addressed content here).
                    match self.backend.resolve(offset) {
                        Some(ResolvedPage::Chunk { hash }) => {
                            let bytes = self.handle.block_on(self.backend.fetch_chunk(hash))?;
                            self.install_chunk_at(offset, bytes, false)?;
                        }
                        Some(ResolvedPage::Canonical { canonical_offset }) => {
                            match self.backend.canonical_chunk_hash(canonical_offset) {
                                Some(hash) => {
                                    let bytes =
                                        self.handle.block_on(self.backend.fetch_chunk(hash))?;
                                    self.install_chunk_at(offset, bytes, false)?;
                                }
                                None => {
                                    if self.base_shm.is_some() {
                                        self.install_zero_substrate(offset, false)?;
                                    } else {
                                        self.install_zero_at(offset, false)?;
                                    }
                                }
                            }
                        }
                        None => {
                            return Err(HandlerError::PeerLost(format!(
                                "AltSource demote for unresolvable offset {offset:#x}"
                            )));
                        }
                    }
                    stats.alt_sourced += 1;
                }
            }
            processed += 1;
            if processed.is_multiple_of(256) {
                if let Some(control) = self.control.as_ref() {
                    control.report(engram_migrate_proto::HandlerControl::DrainProgress {
                        pulled: stats.pulled,
                        remaining: (total_sealed - processed) as u64,
                    });
                }
            }
        }

        let _ = crate::peer::send_drain_done(&mut conn, stats);
        Ok(stats)
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
pub struct RunListenerOpts {
    pub prefault_trace: Option<WorkingSetTrace>,
    pub recorder_window: Duration,
    pub trace_output: Option<PathBuf>,
    pub base_shm: Option<crate::base_shm::BaseShm>,
    /// ADR 0045 C2: post-copy peer mode (session already sealed) +
    /// the optional control-sock reporter.
    pub peer: Option<(
        std::sync::Arc<crate::peer::PeerSession>,
        Option<std::sync::Arc<crate::peer::ControlTx>>,
    )>,
}

pub fn run_listener(
    listen: PathBuf,
    backend: Arc<ChunkedMemoryBackend>,
    handle: TokioHandle,
    opts: RunListenerOpts,
) -> Result<WorkingSetTrace, HandlerError> {
    let RunListenerOpts {
        prefault_trace,
        recorder_window,
        trace_output,
        base_shm,
        peer,
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
    let mut rt = Runtime::new(mappings, uffd, backend, handle, recorder_window, base_shm)?;
    if let Some(path) = trace_output {
        rt.set_trace_output(path);
    }
    if let Some((session, control)) = peer {
        rt.set_peer(session, control);
    }
    // ADR 0043 P1: run the working-set prefault as a BACKGROUND producer
    // concurrent with the fault loop, instead of blocking resume on it.
    // vCPUs run the instant Firecracker resumes; any page the guest touches
    // before the prefault reaches it is served on-demand by the fault loop.
    // This is safe to run in parallel: `install_chunk_at` is idempotent via
    // the per-chunk `installed` bitmap (whichever of prefault/fault installs
    // first wins, the other skips), and `serve_pagefault` explicitly wakes a
    // vCPU whose fault raced ahead of a prefault install (the `wake_page`
    // path). `Uffd` is `Send + Sync` (a `RawFd`) and every install path is
    // mutex-guarded, so sharing the `Runtime` across the two threads is
    // sound. Previously prefault ran to completion before `run()`, which put
    // the entire working-set fetch on the resume critical path.
    let rt = Arc::new(rt);
    let prefault_thread = {
        let rt = Arc::clone(&rt);
        std::thread::Builder::new()
            .name("engram-uffd-prefault".to_string())
            .spawn(move || {
                // ADR 0045 C2: in peer mode the sealed drain runs FIRST —
                // sealed content exists only in the paused source, so
                // draining it is what releases the source, and it makes
                // the trace prefault + sweep below trivially safe (every
                // sealed chunk is installed before their stale resolve()
                // view could touch it). A drain failure is PeerLost:
                // report and stop producing — the host-agent rewinds the
                // whole VM; warming a doomed guest is wasted I/O.
                if let Some(peer) = rt.peer.clone() {
                    let started = std::time::Instant::now();
                    match rt.drain_from_peer(&peer) {
                        Ok(stats) => {
                            let (faults, fault_us, fault_max_us) = peer.fault_stats();
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
                            if let Some(control) = rt.control.as_ref() {
                                control.report(engram_migrate_proto::HandlerControl::DrainDone {
                                    pulled: stats.pulled,
                                    alt_sourced: stats.alt_sourced,
                                    zero_chunks: stats.zero_chunks,
                                    ms: started.elapsed().as_millis() as u64,
                                    faults,
                                    fault_us,
                                    fault_max_us,
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
                // Hot set first (when a trace exists), then the eager
                // full sweep covers everything else — so by the time
                // the guest touches ANY page, the odds it must round-
                // trip the fault loop shrink to the race window. The
                // sweep is what kills the migration-restore crawl: no
                // host trace exists on a fresh dest, and the divergent
                // chunk set otherwise faults in serially (ADR 0045 C1).
                if let Some(trace) = prefault_trace {
                    if let Err(e) = rt.prefault_from_trace(&trace) {
                        tracing::warn!(
                            error = %e,
                            "background prefault failed; remaining pages serve on-demand",
                        );
                    }
                }
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
    // Join the prefault producer before freezing the recorder so it's
    // quiesced. Once the uffd closes (FC exited) the prefault's next install
    // errors out, so this returns promptly rather than blocking teardown.
    if let Ok(handle) = prefault_thread {
        let _ = handle.join();
    }
    result?;
    Ok(rt.finish_recorder())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunked::ChunkedMemoryBackend;
    use engram_chunk_store::cache::{ChunkCache, ChunkCacheConfig};
    use engram_chunk_store::manifest::{
        ChunkRef, ChunkSize, ManifestKind, MANIFEST_SCHEMA_VERSION,
    };
    use engram_chunk_store::{ChunkStore, Manifest};
    use engram_core::traits::BlobStorage;
    use engram_storage_local::LocalBlobStorage;
    use userfaultfd::UffdBuilder;

    /// ADR 0043 P1: the prefault now runs on a background thread CONCURRENT
    /// with the fault loop instead of fully ahead of it. Prove the two install
    /// paths race safely: a `prefault_from_trace` thread and a `serve_pagefault`
    /// thread, with an overlapping chunk range, must between them install every
    /// chunk EXACTLY once (the `installed` bitmap serialises the check-and-set)
    /// and leave the guest region byte-correct — no double-`UFFDIO_COPY`
    /// (`EEXIST`), no corruption, no panic. This is the core safety property the
    /// concurrent restructure relies on (the wake-a-blocked-vCPU half is
    /// `wake_page`, unchanged + covered by the FC UFFD integration tests).
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn concurrent_prefault_and_faults_install_every_chunk_once_and_correctly() {
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
        let mut hashes = Vec::with_capacity(n_chunks);
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
            hashes.push(hash);
        }
        let manifest = Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Memory,
            chunk_size: ChunkSize::bytes(chunk_size),
            total_bytes: total,
            chunks: entries,
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let backend = Arc::new(
            ChunkedMemoryBackend::new(&manifest, &manifest, ChunkCache::new(cfg), store).unwrap(),
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
                Duration::ZERO,
                None,
            )
            .unwrap(),
        );

        // Two install paths race with an OVERLAP in the middle quarter
        // [n/4, 3n/4): the prefault thread covers chunks [0, 3n/4); the
        // "fault server" thread serves chunks [n/4, n). Every chunk is covered
        // by at least one path; the overlap forces the bitmap to arbitrate.
        // Both call `handle.block_on`, so they MUST run on plain threads (not
        // tokio workers) — exactly as the handler runs on a spawn_blocking thread.
        let base = ptr as u64;
        // Build the trace the way production does — observe the first 3/4 of
        // the chunks into a recorder, then freeze it.
        let mut recorder = WorkingSetRecorder::new(1, Duration::from_secs(60));
        for h in hashes.iter().take(3 * n_chunks / 4) {
            recorder.observe(*h);
        }
        let trace = recorder.finish();
        let rt_pf = Arc::clone(&rt);
        let prefault = std::thread::spawn(move || {
            rt_pf.prefault_from_trace(&trace).expect("prefault");
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
            installed.iter().all(|b| *b),
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
        use engram_migrate_proto::{
            read_frame, write_frame, FromSource, SealBitmap, ToSource, PROTO_VERSION,
        };
        use sha2::{Digest, Sha256};

        let page_size = 4096u64;
        let chunk_size = page_size;
        let n_chunks = 8usize;
        let total = chunk_size * n_chunks as u64;
        const PEER_BYTE: u8 = 0xEE;

        // Manifest content: chunk i = byte i+1 (the STALE durable view).
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
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let backend = Arc::new(
            ChunkedMemoryBackend::new(&manifest, &manifest, ChunkCache::new(cfg), store).unwrap(),
        );

        // Fake source: seals chunks {1, 3, 5}; chunk 1 = peer bytes,
        // chunk 3 = ZeroChunk, chunk 5 = AltSource (content matches the
        // durable manifest, so the dest fetches it itself).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
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
                    for i in [1u64, 3, 5] {
                        bitmap.set(i);
                    }
                    write_frame(&mut s, &FromSource::Seal { bitmap }).unwrap();
                    while let Ok(ToSource::NeedAt {
                        req_id,
                        chunk_offset,
                    }) = read_frame::<_, ToSource>(&mut s)
                    {
                        let resp = match chunk_offset / chunk_size {
                            1 => {
                                let raw = vec![PEER_BYTE; chunk_size as usize];
                                let (bytes, lz4) = engram_migrate_proto::compress_page(raw);
                                let sha256: [u8; 32] = Sha256::digest(&bytes).into();
                                FromSource::Page {
                                    req_id,
                                    chunk_offset,
                                    bytes,
                                    sha256,
                                    lz4,
                                }
                            }
                            3 => FromSource::ZeroChunk {
                                req_id,
                                chunk_offset,
                            },
                            5 => FromSource::AltSource {
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
            Duration::ZERO,
            None,
        )
        .unwrap();
        rt.set_peer(Arc::clone(&session), None);
        let rt = Arc::new(rt);

        // Producer order, exactly as run_listener does it: drain, then
        // sweep. Off the tokio workers (block_on inside).
        let rt_drain = Arc::clone(&rt);
        let sess = Arc::clone(&session);
        let stats = std::thread::spawn(move || {
            let stats = rt_drain.drain_from_peer(&sess).expect("drain");
            rt_drain.sweep_all().expect("sweep");
            stats
        })
        .join()
        .unwrap();
        assert_eq!(
            (stats.pulled, stats.zero_chunks, stats.alt_sourced),
            (1, 1, 1),
            "drain accounting"
        );

        // A fault on an already-drained sealed chunk takes the wake
        // path (no peer round-trip, no error).
        let base = ptr as u64;
        rt.serve_pagefault(base + chunk_size)
            .expect("sealed fault post-drain");

        // Every chunk installed; sealed content is the PEER's truth.
        assert!(rt.installed.lock().unwrap().iter().all(|b| *b));
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
}
