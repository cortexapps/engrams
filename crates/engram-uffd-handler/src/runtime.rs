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
use std::sync::Arc;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use bytes::Bytes;
use engram_chunk_store::working_set::WorkingSetTrace;
use sendfd::RecvWithFd;
use tokio::runtime::Handle as TokioHandle;
use userfaultfd::{Event, Uffd};

use crate::chunked::{ChunkedBackendError, ChunkedMemoryBackend, ResolvedPage};
use crate::proto::{GuestRegionUffdMapping, HANDSHAKE_BUF_BYTES};
use crate::working_set::WorkingSetRecorder;

/// ADR 0019 / telemetry restoration (#526): per-jail prefault
/// effectiveness snapshot, written next to `working-set-trace.json` in
/// the same jail dir (same per-jail-file pattern). The host-agent reads
/// it after a restore completes (`pooled_backend.rs`, next to its
/// `working_set_trace_path` accessor) and emits the
/// `engram_resume_prefault_*` counters from it.
///
/// This is the standing detector for "prefault shipped but silently
/// stopped firing" — it went inert three separate, undetected ways
/// (`d0e5ecf3` dead canonical-trace path removed, `cf6e4d32`
/// per-checkpoint manifest key = guaranteed miss, `7c2a7226` publish
/// killed by eviction SIGKILL), each caught only by manual archaeology
/// weeks later. A missing file where a trace was requested (host-agent
/// side: `outcome="stats_missing"`) is now itself the alarm.
///
/// Convergence note: `prefault-admission-control` (landing in the same
/// overhaul) ships a superset gate file (`prefault-gate.json`, same
/// fields + `trace_source`) that reuses this struct's counter
/// namespace. Whichever issue lands second drops its own file in favor
/// of the other's — see #526's guardrails. Do not fork a second
/// jail-dir file or a parallel `engram_prefault_*` namespace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrefaultStats {
    /// Whether a working-set trace was requested AND successfully
    /// loaded for this restore. `false` means there was nothing to
    /// replay (base image, migration dest with no prior life, or the
    /// prior life's publish never landed) — expected, not an error.
    pub trace_loaded: bool,
    /// `trace.chunks.len()` — the size of the trace that was loaded.
    /// `0` when `trace_loaded` is `false`.
    pub chunks_in_trace: usize,
    /// Chunks the prefault actually installed (a position the session
    /// manifest still needs, not already installed by a racing fault).
    pub installed: usize,
    /// Trace entries that were skipped (the session manifest no longer
    /// references that hash — rewritten or GC'd since the trace was
    /// recorded).
    pub skipped: usize,
    /// Wall-clock duration of the prefault pass.
    pub duration_ms: u64,
    /// Review finding 6: issue step (d)3's "uffd-handler live peer
    /// page-serves are recorded in the same per-jail stats file" — the
    /// post-copy migration destination's peer-fill counterpart to
    /// `installed`/`skipped`. `#[serde(default)]` so a reader built
    /// against the pre-finding-6 schema (or a file this process never
    /// patches, e.g. `main.rs`'s early `trace_loaded: false` write on a
    /// non-peer resume) still parses as all-zero, not an error.
    ///
    /// Populated once, as a read-modify-write patch applied at the very
    /// end of `run_listener`'s background thread (strictly after
    /// `prefault_from_trace`/`sweep_all`, so it can never be clobbered
    /// by their own writes) — see `run_listener`. A **snapshot as of
    /// that patch**, not a full-VM-lifetime total: `serve_pagefault`
    /// can still route later on-demand faults to the peer after this
    /// point. Span attributes only for now (`resume.prefault_stats` on
    /// the host-agent side) — `epic-gcs-free-resume` owns promoting
    /// these to their own Prometheus counters.
    #[serde(default)]
    pub peer_pulled: u64,
    /// Chunks the drain sourced from somewhere other than the direct
    /// peer pull (e.g. already-local via a racing fault/prefault) —
    /// `engram_uffd_handler::peer::DrainStats::alt_sourced`.
    #[serde(default)]
    pub peer_alt_sourced: u64,
    /// Zero-filled chunks the drain skipped installing (no bytes to
    /// pull) — `engram_uffd_handler::peer::DrainStats::zero_chunks`.
    #[serde(default)]
    pub peer_zero_chunks: u64,
    /// Live page faults served directly from the peer during the
    /// fault loop, as of the patch point (`Peer::fault_stats().0`) —
    /// distinct from `peer_pulled` (the eager drain), this is the
    /// on-demand path a guest's own touch takes before the drain
    /// reaches that chunk.
    #[serde(default)]
    pub peer_live_faults: u64,
}

/// Filename for [`PrefaultStats`], colocated with
/// [`crate::runtime`]'s per-jail `working-set-trace.json`
/// (`WORKING_SET_TRACE_FILE` in `engram-sandbox-firecracker`).
pub const PREFAULT_STATS_FILE: &str = "prefault-stats.json";

/// Where `PrefaultStats` lands for a jail whose per-jail trace file is
/// `trace_output` — the sibling `<jail_dir>/prefault-stats.json`.
/// `None` only if `trace_output` has no parent directory (never true
/// for a real jail path; guards a degenerate relative path in tests).
pub fn prefault_stats_path(trace_output: &std::path::Path) -> Option<PathBuf> {
    trace_output
        .parent()
        .map(|dir| dir.join(PREFAULT_STATS_FILE))
}

/// Best-effort, atomic (temp+rename) write — same pattern as
/// [`Runtime::dump_trace_output`]. Failures are logged, never
/// propagated: the stats file is a diagnostic surface, not load-bearing
/// for the restore itself (a write failure here must never fail or
/// slow down the guest's boot).
pub fn write_prefault_stats(path: &std::path::Path, stats: &PrefaultStats) {
    match serde_json::to_vec(stats) {
        Ok(bytes) => {
            let tmp = path.with_extension("json.tmp");
            if let Err(e) = std::fs::write(&tmp, &bytes) {
                tracing::warn!(error = %e, path = %tmp.display(), "prefault_stats tmp write");
                return;
            }
            if let Err(e) = std::fs::rename(&tmp, path) {
                tracing::warn!(error = %e, path = %path.display(), "prefault_stats rename");
            }
        }
        Err(e) => tracing::warn!(error = %e, "prefault_stats serialize"),
    }
}

/// Review finding 6: the pure read-modify-write half of patching the
/// peer-drain snapshot onto whatever `prefault-stats.json` bytes already
/// exist (written by `prefault_from_trace` or `main.rs`'s early
/// `trace_loaded: false` case). `existing_bytes` is `None`/unparseable
/// exactly when the handler never wrote a stats file at all (crashed
/// before either writer ran) — deliberately returns `None` in that case
/// rather than fabricating a file, so a genuinely dead handler still
/// alarms as `stats_missing` instead of surfacing a peer-only stub that
/// would mask it.
fn patch_peer_fields(
    existing_bytes: Option<&[u8]>,
    (peer_pulled, peer_alt_sourced, peer_zero_chunks, peer_live_faults): (u64, u64, u64, u64),
) -> Option<PrefaultStats> {
    let mut stats: PrefaultStats = serde_json::from_slice(existing_bytes?).ok()?;
    stats.peer_pulled = peer_pulled;
    stats.peer_alt_sourced = peer_alt_sourced;
    stats.peer_zero_chunks = peer_zero_chunks;
    stats.peer_live_faults = peer_live_faults;
    Some(stats)
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
    /// Per-chunk installation lifecycle (see [`ChunkState`]). Lets the
    /// fault loop skip duplicate `UFFDIO_COPY` attempts for chunks
    /// already installed (`EEXIST` would otherwise force a retry), and
    /// — via the tri-state + `install_cv` — lets a fault that races an
    /// in-flight install *wait* for it instead of waking a missing page.
    /// A failed install rolls the chunk back to `Empty` so it is retried
    /// rather than permanently poisoned (issue #206).
    installed: Mutex<Vec<ChunkState>>,
    /// Notified whenever a chunk leaves `Installing` (→ `Installed` on
    /// success, → `Empty` on rollback). A fault that found the chunk
    /// `Installing` waits on this until the in-flight install settles.
    install_cv: Condvar,
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
            installed: Mutex::new(vec![ChunkState::Empty; chunk_count]),
            install_cv: Condvar::new(),
            page_size,
            trace_output: None,
            base_shm,
            zeropage_ok: std::sync::atomic::AtomicBool::new(true),
            peer: None,
            control: None,
            drain_done: std::sync::atomic::AtomicBool::new(false),
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

    /// The per-jail `prefault-stats.json` sibling of this run's
    /// `--trace-output` path, or `None` when no `--trace-output` was
    /// given (VZ/Process backends never spawn this binary; a test
    /// harness that omits it just gets no stats file — best-effort
    /// diagnostics, never load-bearing).
    fn prefault_stats_path(&self) -> Option<PathBuf> {
        self.trace_output
            .as_ref()
            .and_then(|p| prefault_stats_path(p))
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
        let started = std::time::Instant::now();
        let mut installed = 0usize;
        let mut skipped = 0usize;
        // Run the body in a closure so a `?`-propagated error still falls
        // through to the stats write below (#526: a handler that dies
        // partway through must leave a stats file recording what it DID
        // manage before failing — not the total absence that would
        // masquerade as `outcome="stats_missing"`, the "handler never even
        // started" alarm).
        let result: Result<(), HandlerError> = (|| {
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
            Ok(())
        })();
        tracing::info!(
            installed,
            skipped,
            trace_len = trace.chunks.len(),
            ok = result.is_ok(),
            "prefault from working-set trace complete"
        );
        if let Some(path) = self.prefault_stats_path() {
            write_prefault_stats(
                &path,
                &PrefaultStats {
                    trace_loaded: true,
                    chunks_in_trace: trace.chunks.len(),
                    installed,
                    skipped,
                    duration_ms: started.elapsed().as_millis() as u64,
                    ..Default::default()
                },
            );
        }
        result
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
                    ("canonical", Some(ResolvedPage::Zero { .. })) => {
                        // Zero chunks are skipped — installing them would
                        // privately materialize untouched guest RAM (see
                        // the pass comment above). Counted once, in the
                        // canonical pass, as zero_skipped.
                        deferred += 1;
                        false
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

    /// Atomically claim a chunk for installation (CAS `Empty →
    /// Installing` under the lock). Concurrent prefault / fault / drain
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

    /// Install `bytes` (a full chunk) at the given session byte
    /// offset, copying into the guest's UFFD-registered region via
    /// `UFFDIO_COPY`. `wake` controls whether vCPUs blocked on
    /// pages in this range are woken (`true` during the fault loop,
    /// `false` during prefault when no vCPU is blocked yet).
    ///
    /// Walks every region the chunk touches (see `install_spanning`):
    /// a chunk straddling an FC region boundary is installed in full,
    /// not clamped to the first region.
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

        // Claim the chunk (Empty → Installing). If another installer is
        // mid-flight or already done, short-circuit to a wake (false);
        // a failed in-flight install rolls back to Empty and we retry.
        match self.claim_for_install(chunk_idx) {
            Claim::Claimed => {}
            _ => return Ok(false),
        }

        // The chunk's bytes occupy file offsets
        // [byte_offset, byte_offset + total_len). Walk every region the
        // range touches so a boundary-spanning chunk installs in FULL,
        // copying the matching slice of `bytes` into each segment.
        let total_len = std::cmp::min(chunk_size, bytes.len() as u64);
        let result = self.install_spanning(byte_offset, total_len, |host_va, seg_len, pos| {
            // SAFETY: `bytes[pos..pos+seg_len]` is in bounds
            // (`install_spanning` never advances past `total_len`, which
            // is clamped to `bytes.len()`). `host_va` is a guest-visible
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
        });
        self.settle_install(chunk_idx, &result);
        result.map(|()| true)
    }

    /// Install a zero-filled chunk: `UFFDIO_ZEROPAGE` over the
    /// chunk-aligned range. Used for canonical chunks the manifest
    /// omits (implicit zero pages). No copy — the kernel maps its
    /// shared zero page, COW on the guest's first write. Same
    /// idempotency + clamping discipline as `install_chunk_at`.
    fn install_zero_at(&self, byte_offset: u64, wake: bool) -> Result<bool, HandlerError> {
        let chunk_size = self.backend.chunk_size();
        let chunk_idx = (byte_offset / chunk_size) as usize;

        match self.claim_for_install(chunk_idx) {
            Claim::Claimed => {}
            _ => return Ok(false),
        }

        // Zero the chunk's whole [byte_offset, +total_len) range,
        // walking every region it spans (clamped to the manifest's
        // total so the final partial chunk doesn't overrun).
        let total_len = std::cmp::min(
            chunk_size,
            self.backend.total_bytes().saturating_sub(byte_offset),
        );
        let result = self.install_spanning(byte_offset, total_len, |host_va, seg_len, _pos| {
            // SAFETY: `host_va` is a guest-visible address in a UFFD-
            // registered region we hold zeropage access to. ZEROPAGE
            // installs zero pages over the range + wakes blocked vCPUs
            // atomically per page when `wake`.
            unsafe {
                self.uffd
                    .zeropage(host_va as *mut std::ffi::c_void, seg_len, wake)?;
            }
            Ok(())
        });
        self.settle_install(chunk_idx, &result);
        result.map(|()| true)
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
        match self.claim_for_install(chunk_idx) {
            Claim::Claimed => {}
            _ => return Ok(false),
        }

        let populate_len = std::cmp::min(
            chunk_size,
            self.backend.total_bytes().saturating_sub(byte_offset),
        );
        // All fallible work — the (multi-second, network-bound)
        // `fetch_chunk`, the base write, and the CONTINUE walk — runs in
        // `populate_and_continue` so a single `settle_install` publishes
        // `Installed` only on full success and rolls the claim back to
        // `Empty` on ANY error. The chunk is never left `Installing`
        // across `?`, and a transient fetch failure is retried by the
        // next fault rather than permanently poisoning the chunk (#206).
        let result = self.populate_and_continue(base, byte_offset, populate_len, hash, wake);
        self.settle_install(chunk_idx, &result);
        result.map(|()| true)
    }

    /// The fallible body of `install_canonical_shared`: ensure the base
    /// shm holds the chunk's bytes (fetch + write, skipped when a sibling
    /// already populated the range), then `UFFDIO_CONTINUE` every region
    /// segment the chunk spans. Split out so the claim is settled in one
    /// place on the combined `Result`.
    fn populate_and_continue(
        &self,
        base: &crate::base_shm::BaseShm,
        byte_offset: u64,
        populate_len: u64,
        hash: engram_chunk_store::ChunkHash,
        wake: bool,
    ) -> Result<(), HandlerError> {
        // Ensure the base shm holds this chunk's bytes (file-offset keyed
        // — independent of the region layout) before we map them.
        if !base.is_populated(byte_offset, populate_len) {
            let bytes = self.handle.block_on(self.backend.fetch_chunk(hash))?;
            base.write_chunk(byte_offset, &bytes)
                .map_err(HandlerError::BaseShm)?;
        }

        // CONTINUE each region segment the chunk spans. Per segment the
        // kernel may map a prefix and return progress (looped) and may
        // EEXIST on a page a racing fault already mapped.
        self.install_spanning(byte_offset, populate_len, |host_va, seg_len, _pos| {
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

    /// ADR 0045 substrate (v2b): zero canonical chunks. Try
    /// `UFFDIO_ZEROPAGE` (maps the kernel zero page — zero RAM until the
    /// guest writes); some kernels reject it on MAP_PRIVATE file-backed
    /// mappings, in which case fall back to a private COPY of a zeroed
    /// buffer (correct, costs one private page per touched page).
    fn install_zero_substrate(&self, byte_offset: u64, wake: bool) -> Result<bool, HandlerError> {
        use std::sync::atomic::Ordering;
        let chunk_size = self.backend.chunk_size();
        let chunk_idx = (byte_offset / chunk_size) as usize;
        match self.claim_for_install(chunk_idx) {
            Claim::Claimed => {}
            _ => return Ok(false),
        }

        // Walk every region the chunk spans (clamped to the manifest's
        // total). Per segment try ZEROPAGE; on a kernel that rejects it
        // for this MAP_PRIVATE file mapping, fall back to a COPY of a
        // zeroed buffer for that segment and every later one.
        let total_len = std::cmp::min(
            chunk_size,
            self.backend.total_bytes().saturating_sub(byte_offset),
        );
        let result = self.install_spanning(byte_offset, total_len, |host_va, seg_len, _pos| {
            if self.zeropage_ok.load(Ordering::Relaxed) {
                // SAFETY: range is inside a registered region.
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
                            "UFFDIO_ZEROPAGE unsupported on the substrate mapping;                          falling back to COPY-of-zeros"
                        );
                    }
                    Err(e) => return Err(HandlerError::Uffd(e)),
                }
            }
            let zeros = bytes::Bytes::from(vec![0u8; seg_len]);
            // SAFETY: zeroed buffer of seg_len; same contract as
            // install_chunk_at's copy.
            unsafe {
                self.uffd.copy(
                    zeros.as_ptr() as *const _,
                    host_va as *mut std::ffi::c_void,
                    seg_len,
                    wake,
                )?;
            }
            Ok(())
        });
        self.settle_install(chunk_idx, &result);
        result.map(|()| true)
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
                            // Issue #227 (a): before fatally escalating, re-check
                            // whether the chunk landed meanwhile — the drain or a
                            // sibling fault may have installed it while THIS fault
                            // sat parked mid-redial (a multi-second window), or the
                            // drain may have just finished and torn down the export
                            // out from under us. If it's installed now, the peer
                            // was never actually needed: wake and return Ok rather
                            // than rewinding a healthy destination.
                            if self.chunk_installed(chunk_idx) {
                                tracing::info!(
                                    chunk_idx,
                                    error = %e,
                                    "sealed fault errored but chunk landed via drain/sibling; \
                                     waking instead of escalating PeerLost"
                                );
                                self.wake_page(page_aligned, page_size)?;
                                return Ok(());
                            }
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
        }

        match self
            .backend
            .resolve(chunk_byte_offset)
            .ok_or(HandlerError::AddressOutsideRegions(fault_addr))?
        {
            ResolvedPage::Canonical { canonical_offset } => {
                // Route B: "canonical" means "fetch the canonical chunk
                // hash from the store" (shared via base-shm CONTINUE
                // when present). Reaching this arm means the session
                // agrees with the base here, so a hash is expected; the
                // `None` branch is a defensive zero-fill for a malformed
                // manifest — session omissions resolve to `Zero` below.
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
            ResolvedPage::Zero { offset } => {
                // Session omits this offset → authoritative zero page,
                // regardless of what the base holds there (the session
                // capture is full + zero-omitted). No chunk to record.
                let installed = if self.base_shm.is_some() {
                    self.install_zero_substrate(offset, true)?
                } else {
                    self.install_zero_at(offset, true)?
                };
                if !installed {
                    self.wake_page(page_aligned, page_size)?;
                }
            }
        }

        Ok(())
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

    /// Latch the drain-complete flag (DrainDone). Once set, the fault
    /// path stops dialing the peer (see `drain_done` field doc).
    fn mark_drain_done(&self) {
        self.drain_done
            .store(true, std::sync::atomic::Ordering::SeqCst);
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
        hot: Option<&engram_chunk_store::working_set::WorkingSetTrace>,
    ) -> Result<crate::peer::DrainStats, HandlerError> {
        use crate::peer::{DrainStats, PeerPage};

        const PIPELINE_DEPTH: usize = 8;
        // Per-request (retryable, `Error{req_id: Some}`) drain failures get
        // a bounded same-conn retry before the drain gives up (issue #227 b).
        const DRAIN_REQUEST_RETRIES: u32 = 3;
        const DRAIN_REQUEST_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

        let chunk_size = self.backend.chunk_size();
        let seal = peer.seal();
        let mut stats = DrainStats::default();
        let mut conn = peer
            .open_extra_conn()
            .map_err(|e| HandlerError::PeerLost(format!("drain conn: {e}")))?;
        let next_req = std::sync::atomic::AtomicU64::new(1_000_000); // distinct from fault-conn ids in logs

        // Pending sealed chunk offsets, skipping anything already
        // installed by a racing fault. HOT-FIRST: the source's
        // resume-time working set (in first-fault order) leads, so
        // the chunks the guest touches first are installed first and
        // most would-be faults are already local when they happen —
        // the no-added-pause version of a hot-set pre-install. The
        // cold remainder keeps offset order.
        let mut todo: Vec<u64> = Vec::with_capacity(seal.count_ones() as usize);
        let mut queued = std::collections::HashSet::new();
        if let Some(trace) = hot {
            for hash in &trace.chunks {
                for offset in self.backend.session_positions_of(*hash) {
                    let idx = offset / chunk_size;
                    if seal.get(idx) && queued.insert(idx) {
                        todo.push(idx * chunk_size);
                    }
                }
            }
        }
        let hot_leading = todo.len();
        for i in (0..seal.chunk_count).filter(|i| seal.get(*i)) {
            if !queued.contains(&i) {
                todo.push(i * chunk_size);
            }
        }
        if hot_leading > 0 {
            tracing::info!(hot_leading, total = todo.len(), "drain ordered hot-first");
        }
        let total_sealed = todo.len();

        let mut in_flight: std::collections::VecDeque<(u64, u64)> = Default::default(); // (req_id, offset)
        let mut iter = todo.into_iter();
        // Issue #227 (b): offsets the source answered with a per-request
        // error (`Error{req_id: Some}`, e.g. a transient process_vm_readv
        // EAGAIN/ENOMEM). Re-queued for a bounded re-request through the
        // SAME pipeline (preserving strict request/response FIFO order on
        // the wire — never a side-band read) rather than rewinding the
        // whole drain. `attempts[offset]` bounds the retries.
        let mut retry: std::collections::VecDeque<u64> = Default::default();
        let mut attempts: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
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
                // Drain re-queued (per-request-failed) offsets first, then
                // the fresh todo list.
                let offset = match retry.pop_front() {
                    Some(o) => o,
                    None => match iter.next() {
                        Some(o) => o,
                        None => break,
                    },
                };
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
                Err(e) => return Err(HandlerError::PeerLost(format!("drain decode: {e}"))),
            };
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
                        Some(ResolvedPage::Zero { .. }) => {
                            // Session omits this offset → private zero
                            // page (the shared base is never written
                            // with session-addressed content here).
                            if self.base_shm.is_some() {
                                self.install_zero_substrate(offset, false)?;
                            } else {
                                self.install_zero_at(offset, false)?;
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
                // Review finding 6: (pulled, alt_sourced, zero_chunks, live
                // faults) from the peer drain, captured below if this
                // restore is peer mode — `None` otherwise (base/resume
                // restores never set `rt.peer`). Patched onto
                // `prefault-stats.json` at the end of this closure.
                let mut peer_drain_stats: Option<(u64, u64, u64, u64)> = None;
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
                    match rt.drain_from_peer(&peer, prefault_trace.as_ref()) {
                        Ok(stats) => {
                            // Issue #227 (a): latch drain-complete BEFORE
                            // reporting DrainDone. The host-agent acts on
                            // DrainDone by committing the migration (which
                            // tears the source export down), so the fault
                            // path must already know not to dial the peer by
                            // the time that frame is observed.
                            rt.mark_drain_done();
                            let (faults, fault_us, fault_max_us) = peer.fault_stats();
                            // Review finding 6: stash the drain + live-fault
                            // snapshot so it can be patched onto
                            // `prefault-stats.json` once every other writer
                            // in this closure has had its turn (see the
                            // read-modify-write patch below `sweep_all`).
                            peer_drain_stats =
                                Some((stats.pulled, stats.alt_sourced, stats.zero_chunks, faults));
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
                // Review finding 6: patch the peer-drain snapshot onto
                // `prefault-stats.json`, read-modify-write. Runs strictly
                // after `prefault_from_trace`'s own internal write (just
                // above) and after `main.rs`'s early `trace_loaded: false`
                // write (which happens before this thread even starts) —
                // sequential code on this one background thread, so this is
                // always the LAST writer and can never be clobbered by an
                // earlier one. A missing/corrupt file at this point (the
                // handler died before either wrote) is left alone: patching
                // a stats-missing state into a fabricated file would defeat
                // the alarm finding 1 exists to protect.
                if let Some(peer_drain_stats) = peer_drain_stats {
                    if let Some(path) = rt.prefault_stats_path() {
                        let existing = std::fs::read(&path).ok();
                        if let Some(patched) =
                            patch_peer_fields(existing.as_deref(), peer_drain_stats)
                        {
                            write_prefault_stats(&path, &patched);
                        }
                    }
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

    /// ADR 0019 / telemetry restoration (#526): `PrefaultStats` is the
    /// per-jail file the host-agent reads post-restore to derive the
    /// `engram_resume_prefault_*` counters — a pure serialize/write/read
    /// round-trip, no uffd/kernel surface involved.
    #[test]
    fn prefault_stats_serialize_write_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let trace_output = dir.path().join("working-set-trace.json");
        let stats_path = prefault_stats_path(&trace_output).expect("sibling path resolves");
        assert_eq!(stats_path, dir.path().join(PREFAULT_STATS_FILE));

        let stats = PrefaultStats {
            trace_loaded: true,
            chunks_in_trace: 42,
            installed: 40,
            skipped: 2,
            duration_ms: 1234,
            peer_pulled: 0,
            peer_alt_sourced: 0,
            peer_zero_chunks: 0,
            peer_live_faults: 0,
        };
        write_prefault_stats(&stats_path, &stats);

        let bytes = std::fs::read(&stats_path).expect("stats file must exist after write");
        let read_back: PrefaultStats =
            serde_json::from_slice(&bytes).expect("stats file must be valid JSON");
        assert_eq!(read_back, stats);

        // The temp file from the atomic write must not linger.
        assert!(
            !stats_path.with_extension("json.tmp").exists(),
            "temp file must be renamed away, not left behind"
        );
    }

    /// The `trace_loaded: false` shape main.rs writes for the no-trace
    /// case (either no trace was requested, or the requested one failed
    /// to load) — distinguishable from a `stats_missing` (file absent
    /// entirely) read on the host-agent side.
    #[test]
    fn prefault_stats_no_trace_shape_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let trace_output = dir.path().join("working-set-trace.json");
        let stats_path = prefault_stats_path(&trace_output).unwrap();

        let stats = PrefaultStats {
            trace_loaded: false,
            ..Default::default()
        };
        write_prefault_stats(&stats_path, &stats);

        let read_back: PrefaultStats =
            serde_json::from_slice(&std::fs::read(&stats_path).unwrap()).unwrap();
        assert!(!read_back.trace_loaded);
        assert_eq!(read_back.chunks_in_trace, 0);
        assert_eq!(read_back.installed, 0);
        assert_eq!(read_back.skipped, 0);
    }

    /// Review finding 6 regression test: the peer-drain snapshot patches
    /// onto an EXISTING stats file (either shape `prefault_from_trace` or
    /// `main.rs`'s no-trace path could have written) without disturbing
    /// the fields that writer already set.
    #[test]
    fn patch_peer_fields_preserves_existing_trace_fields() {
        let base = PrefaultStats {
            trace_loaded: true,
            chunks_in_trace: 10,
            installed: 8,
            skipped: 2,
            duration_ms: 55,
            ..Default::default()
        };
        let bytes = serde_json::to_vec(&base).unwrap();
        let patched = patch_peer_fields(Some(&bytes), (3, 1, 4, 7)).expect("existing file parses");
        assert_eq!(
            patched,
            PrefaultStats {
                trace_loaded: true,
                chunks_in_trace: 10,
                installed: 8,
                skipped: 2,
                duration_ms: 55,
                peer_pulled: 3,
                peer_alt_sourced: 1,
                peer_zero_chunks: 4,
                peer_live_faults: 7,
            },
        );
    }

    /// A handler that died before either writer ran (no stats file at
    /// all) must NOT get a fabricated peer-only file — that would mask
    /// the `stats_missing` alarm finding 1 exists to protect.
    #[test]
    fn patch_peer_fields_none_when_no_prior_writer_ran() {
        assert!(patch_peer_fields(None, (1, 2, 3, 4)).is_none());
        assert!(patch_peer_fields(Some(b"not json"), (1, 2, 3, 4)).is_none());
    }

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
        use engram_migrate_proto::{
            read_frame, write_frame, FromSource, SealBitmap, ToSource, PROTO_VERSION,
        };
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
            let stats = rt_drain.drain_from_peer(&sess, None).expect("drain");
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
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let backend = Arc::new(
            ChunkedMemoryBackend::new(&manifest, &manifest, ChunkCache::new(cfg), store).unwrap(),
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
            Duration::ZERO,
            None,
        )
        .unwrap();
        rt.set_peer(Arc::clone(&session), None);
        let rt = Arc::new(rt);

        let rt_drain = Arc::clone(&rt);
        let sess = Arc::clone(&session);
        let stats = std::thread::spawn(move || rt_drain.drain_from_peer(&sess, None))
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
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let backend = Arc::new(
            ChunkedMemoryBackend::new(&manifest, &manifest, ChunkCache::new(cfg), store).unwrap(),
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
            Duration::ZERO,
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
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let backend = Arc::new(
            ChunkedMemoryBackend::new(&manifest, &manifest, ChunkCache::new(cfg), store).unwrap(),
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
                Duration::ZERO,
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

    /// Build a minimal single-page-per-chunk Runtime with NO real UFFD
    /// region wired to the guest — only the install bookkeeping is
    /// exercised. The UFFD is created over a throwaway anonymous page so
    /// `Runtime::new` has a valid fd; tests here never call the install
    /// ioctls, only `claim_chunk`/`mark_installed`/`release_chunk`/
    /// `wait_until_settled`/`chunk_installed`. Returns `None` if the
    /// kernel forbids unprivileged uffd (so the test SKIPs, like the
    /// real-region tests above).
    fn bookkeeping_runtime(n_chunks: usize) -> Option<Arc<Runtime>> {
        let page_size = 4096u64;
        let chunk_size = page_size;
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
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let backend = Arc::new(
            ChunkedMemoryBackend::new(&manifest, &manifest, ChunkCache::new(cfg), store).unwrap(),
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
                Duration::ZERO,
                None,
            )
            .unwrap(),
        ))
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
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        // Keep a handle to the (initially empty) backing store so the
        // test can land the content between the failing and retrying
        // faults, simulating the transient store error clearing.
        let store_handle = store.clone();
        let backend = Arc::new(
            ChunkedMemoryBackend::new(&manifest, &manifest, ChunkCache::new(cfg), store).unwrap(),
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
                Duration::ZERO,
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
