//! Linux NBD server loop + kernel `/dev/nbdN` orchestration.
//!
//! The host-agent calls [`spawn`] with an allocated `/dev/nbdN`
//! path and a [`ChunkedDiskBackend`]. The function:
//!
//! 1. Opens `/dev/nbdN` (O_RDWR).
//! 2. Creates a `socketpair(AF_UNIX, SOCK_STREAM)` — one end goes
//!    to the kernel via `NBD_SET_SOCK`, the other stays in-process
//!    so the daemon can serve requests over it.
//! 3. Calls a sequence of NBD ioctls to size the block device:
//!    `NBD_SET_BLKSIZE` (4096), `NBD_SET_SIZE_BLOCKS` (total /
//!    4096), `NBD_SET_FLAGS` (HAS_FLAGS | SEND_FLUSH | SEND_TRIM),
//!    `NBD_SET_SOCK`.
//! 4. Spawns a dedicated OS thread that calls `NBD_DO_IT` — this
//!    syscall blocks until the kernel sees a disconnect, and the
//!    kernel won't accept I/O until something is in this loop.
//! 5. Spawns a tokio task that reads NBD requests over the
//!    server-side `UnixStream`, dispatches to the backend, and
//!    writes replies. Each request is served sequentially —
//!    in-flight pipelining is a follow-up optimization (the kernel
//!    side handles many handles concurrently but a sequential
//!    serve is correct).
//!
//! Shutdown: drop the returned [`NbdHandle`] to tear down. The
//! `Drop` impl issues `NBD_DISCONNECT` (which unblocks
//! `NBD_DO_IT`), aborts the serve task, joins the kernel thread,
//! and `NBD_CLEAR_SOCK` to release the kernel-side fd reference.
//!
//! `unsafe` blocks are the unavoidable kernel-syscall surface
//! (raw `libc::ioctl`, `libc::socketpair`, fd ownership transfer).
//! Each is annotated.

#![cfg(target_os = "linux")]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream as TokioUnixStream;
use tokio::task::JoinHandle as TokioJoinHandle;

use super::backend::{ChunkedDiskBackend, DiskBackendError};
use super::nbd::{NbdCommand, NbdReply, NbdRequest, REQUEST_HEADER_LEN};
use super::slot::{NbdSlot, NbdSlotAllocator};

// ---------------------------------------------------------------------
// NBD ioctl constants
// ---------------------------------------------------------------------
//
// Per `<linux/nbd.h>`:
//   #define NBD_SET_SOCK         _IO(0xab, 0)
//   #define NBD_SET_BLKSIZE      _IO(0xab, 1)
//   #define NBD_SET_SIZE         _IO(0xab, 2)
//   #define NBD_DO_IT            _IO(0xab, 3)
//   #define NBD_CLEAR_SOCK       _IO(0xab, 4)
//   #define NBD_SET_SIZE_BLOCKS  _IO(0xab, 7)
//   #define NBD_DISCONNECT       _IO(0xab, 8)
//   #define NBD_SET_TIMEOUT      _IO(0xab, 9)
//   #define NBD_SET_FLAGS        _IO(0xab, 10)
//
// `_IO(type, nr)` on Linux is `((type) << _IOC_TYPESHIFT) |
// ((nr) << _IOC_NRSHIFT)` where TYPESHIFT=8, NRSHIFT=0. So:
//
// The `| 0` / `<< 0` below are deliberate visual alignment with the
// `_IO(0xab, N)` source convention — clippy's identity_op fires
// even though removing them would change nothing.

// Typed as `libc::Ioctl` (NOT `u64`) so the same `libc::ioctl` call
// site type-checks on glibc (`Ioctl = c_ulong`) AND musl
// (`Ioctl = c_int`). The OSS CI cross-musl lane only covers
// agentd/bootstrap/harness binaries today, so musl-build of the
// host-agent first showed this gap in the bake CI. All these NBD
// command numbers are small (`(0xab << 8) | N`, max ~44_000) so the
// values fit either width without a cast.
#[allow(clippy::identity_op)]
const NBD_SET_SOCK: libc::Ioctl = (0xab << 8) | 0;
const NBD_SET_BLKSIZE: libc::Ioctl = (0xab << 8) | 1;
const NBD_DO_IT: libc::Ioctl = (0xab << 8) | 3;
const NBD_CLEAR_SOCK: libc::Ioctl = (0xab << 8) | 4;
const NBD_SET_SIZE_BLOCKS: libc::Ioctl = (0xab << 8) | 7;
const NBD_DISCONNECT: libc::Ioctl = (0xab << 8) | 8;
const NBD_SET_TIMEOUT: libc::Ioctl = (0xab << 8) | 9;
const NBD_SET_FLAGS: libc::Ioctl = (0xab << 8) | 10;

/// `NBD_FLAG_HAS_FLAGS` bit. Required so the kernel honours the
/// other capability bits we set. From `<linux/nbd.h>`.
#[allow(clippy::identity_op)]
const NBD_FLAG_HAS_FLAGS: u32 = 1 << 0;
/// `NBD_FLAG_SEND_FLUSH`. Tells the kernel `NBD_CMD_FLUSH` is
/// available so fsync()s inside the guest translate into our
/// daemon-side FLUSH dispatch.
const NBD_FLAG_SEND_FLUSH: u32 = 1 << 2;
/// `NBD_FLAG_SEND_TRIM`. The guest's discard / fstrim flows
/// through as `NBD_CMD_TRIM`.
const NBD_FLAG_SEND_TRIM: u32 = 1 << 5;

/// Block size the daemon hard-pins. 4096 matches the kernel's
/// page size on x86_64 and the chunk-aligned units we serve.
/// `NBD_SET_SIZE_BLOCKS` uses this as its unit.
pub const NBD_BLOCK_SIZE: u64 = 4096;

/// Kernel-side NBD request timeout in seconds, set via
/// `NBD_SET_TIMEOUT` during the startup dance.
///
/// This is the second of two fail-fast layers. The first is the
/// daemon's own chunk-fetch retry budget (`CHUNK_FETCH_*` in
/// `backend.rs`): a stalled or missing chunk makes `backend.read`
/// return `Err` in bounded time (~tens of seconds), which the serve
/// loop turns into EIO. The kernel timeout backstops the cases that
/// budget can't see — a wedged serve loop, a lock deadlock, or the
/// daemon dying outright — where the daemon never sends *any* reply.
/// Without it the kernel waits forever, leaving the guest's I/O
/// (notably the device-open / partition-probe read at attach) in
/// uninterruptible (`D`-state) sleep, which cascades into a jbd2
/// D-state and an FC pause timeout. With it the kernel times the
/// request out and returns EIO to the guest instead.
///
/// Set comfortably above the daemon's worst-case *legitimate* reply
/// (a 2-chunk op each riding the full retry budget is ~tens of
/// seconds) so it never kills a request that's still making progress.
/// Override via `ENGRAM_NBD_KERNEL_TIMEOUT_SECS`; E2B uses a
/// comparable ~90s ceiling.
fn nbd_kernel_timeout_secs() -> u64 {
    std::env::var("ENGRAM_NBD_KERNEL_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(90)
}

/// Anything that can go wrong launching or running the daemon.
#[derive(Debug)]
pub enum NbdRuntimeError {
    Io(io::Error),
    /// `total_bytes` from the manifest isn't a multiple of
    /// `NBD_BLOCK_SIZE`. The kernel side wants whole-block sizing;
    /// rather than rounding (which would silently expose padding
    /// to the guest), surface as an error so the caller fixes the
    /// manifest.
    UnalignedSize {
        total_bytes: u64,
        block_size: u64,
    },
    /// `ioctl(NBD_DO_IT)` exited unexpectedly (the kernel returns
    /// 0 on disconnect; non-zero means the device disappeared or
    /// the kernel-side socket closed prematurely).
    KernelLoopExited(i32),
}

impl std::fmt::Display for NbdRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::UnalignedSize {
                total_bytes,
                block_size,
            } => write!(
                f,
                "manifest total_bytes={total_bytes} not aligned to NBD block_size={block_size}"
            ),
            Self::KernelLoopExited(rc) => {
                write!(f, "NBD_DO_IT returned {rc}; expected 0 (clean disconnect)")
            }
        }
    }
}

impl std::error::Error for NbdRuntimeError {}

impl From<io::Error> for NbdRuntimeError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<DiskBackendError> for NbdRuntimeError {
    fn from(e: DiskBackendError) -> Self {
        Self::Io(io::Error::other(e))
    }
}

/// Live NBD daemon handle. Holds the spawned tokio serve task,
/// the kernel-side ioctl thread, and the `/dev/nbdN` file
/// descriptor. Dropping cleanly tears everything down.
pub struct NbdHandle {
    nbd_device: PathBuf,
    /// Kept alive for the lifetime of the daemon. Dropping closes
    /// the kernel-side fd, which the kernel reacts to by tearing
    /// down the block device.
    nbd_fd: Option<OwnedFd>,
    /// Background tokio task running the NBD serve loop. Aborted
    /// on `Drop`.
    serve_task: Option<TokioJoinHandle<()>>,
    /// OS thread blocked in `NBD_DO_IT`. Joined on `Drop` after
    /// `NBD_DISCONNECT` releases the kernel-side wait.
    kernel_thread: Option<JoinHandle<()>>,
}

impl NbdHandle {
    /// The path the daemon is bound to. Pass this as FC's
    /// `path_on_host` for the rootfs drive.
    pub fn device_path(&self) -> &Path {
        &self.nbd_device
    }
}

impl Drop for NbdHandle {
    fn drop(&mut self) {
        // ADR 0017 Phase A: tear-down used to run the
        // `kernel_thread.join()` inline, which blocks the calling
        // tokio worker thread until the kernel-side NBD_DO_IT
        // loop exits. When FC is SIGKILLed and the virtio-blk
        // backend leaves in-flight I/O against /dev/nbdN, the
        // kernel doesn't release NBD_DO_IT even after
        // NBD_DISCONNECT — the join blocks indefinitely, and
        // each destroy locks one tokio worker. With ~4 worker
        // threads on prod hosts, four destroys are enough to
        // stall the runtime (no heartbeat, no gRPC, coord
        // declares the host dead). Observed on dev-vm 2026-05-24.
        //
        // Fix: do the cheap synchronous steps inline (NBD_DISCONNECT
        // + serve-task abort) so the kernel side has its
        // shutdown signal, then move the join + CLEAR_SOCK + fd
        // close into a detached `std::thread::spawn`. Drop returns
        // immediately; the kernel-side cleanup completes in the
        // background. If the kernel never exits NBD_DO_IT (the
        // ungraceful-FC case), this thread leaks rather than
        // wedging the runtime. The `/dev/nbdN` path stays
        // "kernel-busy" — the pool allocator (commit 1, this
        // ADR) probes /sys/block/nbdN/pid on acquire so a busy
        // slot is structurally invisible until it's actually
        // recovered (by NBD_DISCONNECT completing, or by the
        // startup cleanup in Phase B of this ADR).
        let device_path = self.nbd_device.clone();
        if let Some(fd) = self.nbd_fd.as_ref() {
            // SAFETY: fd is owned by this struct; ioctl with a
            // direction-less command + no argument is the kernel's
            // documented shutdown path. Errors are logged + ignored.
            let raw = fd.as_raw_fd();
            let rc = unsafe { libc::ioctl(raw, NBD_DISCONNECT) };
            if rc != 0 {
                tracing::warn!(
                    rc,
                    errno = io::Error::last_os_error().raw_os_error(),
                    device = %device_path.display(),
                    "NBD_DISCONNECT ioctl failed during shutdown",
                );
            }
        }
        if let Some(task) = self.serve_task.take() {
            task.abort();
        }
        // Move the kernel-thread join + CLEAR_SOCK + fd close into
        // a detached std::thread so Drop returns immediately. The
        // closure takes ownership of:
        //   - kernel_thread (a JoinHandle<()>)
        //   - nbd_fd (OwnedFd; close-on-drop)
        let kernel_thread = self.kernel_thread.take();
        let nbd_fd = self.nbd_fd.take();
        if kernel_thread.is_some() || nbd_fd.is_some() {
            std::thread::Builder::new()
                .name(format!(
                    "nbd-detach-{}",
                    device_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "?".into())
                ))
                .spawn(move || {
                    if let Some(t) = kernel_thread {
                        if let Err(panic) = t.join() {
                            tracing::warn!(
                                ?panic,
                                device = %device_path.display(),
                                "NBD kernel thread panicked",
                            );
                        }
                    }
                    if let Some(fd) = nbd_fd.as_ref() {
                        // SAFETY: fd still owned by the closure;
                        // CLEAR_SOCK releases the kernel's reference
                        // to the socketpair half we handed it.
                        // Without this the kernel may keep the fd
                        // alive past Drop.
                        let raw = fd.as_raw_fd();
                        let rc = unsafe { libc::ioctl(raw, NBD_CLEAR_SOCK) };
                        if rc != 0 {
                            tracing::debug!(
                                rc,
                                errno = io::Error::last_os_error().raw_os_error(),
                                device = %device_path.display(),
                                "NBD_CLEAR_SOCK ioctl failed (typically harmless on disconnect)",
                            );
                        }
                    }
                    // OwnedFd's Drop closes the device file when
                    // `nbd_fd` goes out of scope at end of closure.
                    drop(nbd_fd);
                    tracing::debug!(
                        device = %device_path.display(),
                        "NBD detached cleanup complete",
                    );
                })
                .ok(); // best-effort; if spawn fails, the cleanup is lost (acceptable — process is likely on its way out)
        }
    }
}

/// ADR 0017 Phase B: probe each device in `paths` for stale
/// kernel-side bindings (a populated `/sys/block/nbdN/pid` pointing
/// at a process that's no longer alive — the usual aftermath of an
/// ungraceful host-agent exit). For each, open the device + issue
/// NBD_DISCONNECT and NBD_CLEAR_SOCK to force the kernel to release.
/// Returns `(probed, recovered, still_stuck)`.
///
/// Best-effort: a recovery that doesn't clear the pid file is
/// logged with `tracing::warn!` so prod ops sees how many devices
/// can't be recovered automatically (operator fallback: reboot the
/// host). The pool-acquire path's `nbd_kernel_busy` probe will
/// continue to skip still-stuck devices, so they're structurally
/// invisible until the kernel releases (often "never" without a
/// reboot).
///
/// Called once at host-agent startup from the NbdSlotAllocator's
/// construction site, BEFORE any new sandboxes attach. Each call
/// emits a one-line `recovered N/M stale NBD devices` summary so
/// ops can monitor cleanup accumulation across restarts.
pub fn recover_stuck_nbd_devices(paths: &[std::path::PathBuf]) -> (usize, usize, usize) {
    let mut probed = 0;
    let mut recovered = 0;
    let mut still_stuck = 0;
    for path in paths {
        match recover_one_stuck_device(path) {
            Ok(NbdRecoveryOutcome::NotStuck) => {}
            Ok(NbdRecoveryOutcome::Recovered) => {
                probed += 1;
                recovered += 1;
            }
            Ok(NbdRecoveryOutcome::StillStuck) => {
                probed += 1;
                still_stuck += 1;
            }
            Err(e) => {
                probed += 1;
                still_stuck += 1;
                tracing::debug!(
                    device = %path.display(),
                    error = %e,
                    "NBD recovery: error during probe; treating as still-stuck",
                );
            }
        }
    }
    if probed > 0 {
        tracing::warn!(
            probed,
            recovered,
            still_stuck,
            "NBD startup cleanup: recovered {recovered} stale NBD devices (of {probed} probed; {still_stuck} still stuck)",
        );
    }
    (probed, recovered, still_stuck)
}

enum NbdRecoveryOutcome {
    NotStuck,
    Recovered,
    StillStuck,
}

fn recover_one_stuck_device(path: &std::path::Path) -> io::Result<NbdRecoveryOutcome> {
    // 1. Probe /sys/block/nbdN/pid. Empty / absent → device isn't bound; nothing to do.
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Ok(NbdRecoveryOutcome::NotStuck);
    };
    let pid_path = format!("/sys/block/{name}/pid");
    let bound_pid = match std::fs::read_to_string(&pid_path) {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return Ok(NbdRecoveryOutcome::NotStuck),
    };

    tracing::warn!(
        device = %path.display(),
        bound_pid = %bound_pid,
        "NBD recovery: device kernel-bound (possibly to a dead pid); attempting recovery via NBD_DISCONNECT + NBD_CLEAR_SOCK",
    );

    // 2. Open the device R/W to get a fd we can ioctl against.
    let fd = OwnedFd::from(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?,
    );
    let raw = fd.as_raw_fd();

    // 3. NBD_DISCONNECT signals the kernel to exit NBD_DO_IT.
    let rc = unsafe { libc::ioctl(raw, NBD_DISCONNECT) };
    if rc != 0 {
        tracing::debug!(
            device = %path.display(),
            errno = io::Error::last_os_error().raw_os_error(),
            "NBD_DISCONNECT in recovery returned non-zero (often harmless: kernel was already disconnected)",
        );
    }

    // 4. NBD_CLEAR_SOCK clears the kernel's reference to whatever
    //    socket the dead daemon registered. Load-bearing on devices
    //    whose bound daemon died without disconnect: the kernel
    //    holds onto the socket ref-count and won't release the
    //    device until cleared.
    let rc = unsafe { libc::ioctl(raw, NBD_CLEAR_SOCK) };
    if rc != 0 {
        tracing::debug!(
            device = %path.display(),
            errno = io::Error::last_os_error().raw_os_error(),
            "NBD_CLEAR_SOCK in recovery returned non-zero",
        );
    }

    // 5. Brief sleep so the kernel has a chance to release. Observed
    //    100ms is sufficient on dev-vm; production may need more on
    //    a heavily loaded host but this is a one-shot startup
    //    operation so we don't iterate.
    std::thread::sleep(std::time::Duration::from_millis(100));

    // 6. Re-probe pid file. Empty → recovered; non-empty → still
    //    stuck (reboot likely required).
    let final_pid = std::fs::read_to_string(&pid_path).unwrap_or_default();
    if final_pid.trim().is_empty() {
        tracing::info!(
            device = %path.display(),
            prior_pid = %bound_pid,
            "NBD recovery: device released",
        );
        Ok(NbdRecoveryOutcome::Recovered)
    } else {
        tracing::warn!(
            device = %path.display(),
            prior_pid = %bound_pid,
            post_recovery_pid = %final_pid.trim(),
            "NBD recovery: device STILL bound after NBD_DISCONNECT + NBD_CLEAR_SOCK; operator may need to reboot the host to recover this slot",
        );
        Ok(NbdRecoveryOutcome::StillStuck)
    }
}

/// Per-sandbox NBD state. Composes everything `PooledBackend`
/// needs to track for a sandbox whose rootfs is served via NBD:
/// the data plane (used for snapshot flush), the kernel-binding
/// handle (Drop tears down), and the slot lease (Drop returns
/// the `/dev/nbdN` path to the pool).
///
/// Field order matters: `Drop` runs top-to-bottom, so the
/// scheduler is cancelled FIRST (ADR 0016 Phase B — the spawned
/// task holds an `Arc<ChunkedDiskBackend>` and might be mid-flush;
/// `abort()` pre-empts cleanly at the next await), THEN the `handle`
/// disconnects the NBD daemon, and finally the `slot` returns to
/// the pool — so a follow-up `acquire()` against the same path
/// doesn't race the kernel's tear-down. The scheduler field is
/// `Option` because (a) commit 2 installs it post-create via
/// [`Self::install_flush_scheduler`] once the sandbox_id is known,
/// and (b) tests / callers that don't want continuous flush leave
/// it `None`.
pub struct NbdSandboxState {
    /// ADR 0016 Phase B continuous flush. `Some` once
    /// `install_flush_scheduler` has been called (cold create,
    /// resume, restart rehydration). `None` until then, and `None`
    /// permanently for callers that opt out (env var, tests).
    pub scheduler: Option<crate::disk_daemon::FlushSchedulerHandle>,
    /// `flush()` produces the new manifest version on snapshot.
    pub backend: Arc<ChunkedDiskBackend>,
    /// Live daemon. Owns the OS thread + Tokio serve task.
    pub handle: NbdHandle,
    /// Slot lease. Returns to the pool when dropped.
    pub slot: NbdSlot,
}

impl NbdSandboxState {
    /// Spawn the flush scheduler for this sandbox and hand the
    /// resulting handle to the `scheduler` field. Idempotent on
    /// already-installed schedulers (replaces — the prior handle's
    /// `Drop` aborts the prior task). Skip when `config.enabled ==
    /// false` so the kill-switch decision lives at one point.
    ///
    /// Called from the cold-create site in `PooledBackend` AFTER
    /// `inner.create()` returns the sandbox_id; the resume + restart
    /// call sites use the same helper.
    pub fn install_flush_scheduler(
        &mut self,
        sandbox_id: engram_core::SandboxId,
        publisher: Arc<dyn crate::disk_daemon::LiveManifestPublisher>,
        config: crate::disk_daemon::FlushSchedulerConfig,
    ) {
        if !config.enabled {
            return;
        }
        let handle = crate::disk_daemon::FlushScheduler::spawn(
            sandbox_id,
            self.backend.clone(),
            publisher,
            config,
        );
        self.scheduler = Some(handle);
    }
}

impl NbdSandboxState {
    /// Convenience: the `/dev/nbdN` path FC should attach as
    /// `path_on_host` for the rootfs drive.
    pub fn device_path(&self) -> &Path {
        self.slot.path()
    }
}

/// One-call setup for a sandbox's NBD-backed rootfs:
/// 1. Build a [`ChunkedDiskBackend`] from `disk_manifest_ref`
///    (reading the manifest from the chunk store) with the Phase B
///    threshold-notify wired.
/// 2. Acquire a `/dev/nbdN` slot from `slot_pool`.
/// 3. [`spawn`] the daemon against the acquired device.
///
/// Returns the composite [`NbdSandboxState`] the caller stores for
/// the sandbox's lifetime; `scheduler` starts as `None`. The caller
/// calls [`NbdSandboxState::install_flush_scheduler`] once
/// `sandbox_id` is known (after `inner.create()` / `inner.restore()`).
/// Dropping the state tears the whole daemon down (scheduler →
/// handle → slot).
///
/// `threshold_bytes` is the dirty-bytes threshold beyond which the
/// backend pokes its `Notify` to wake the scheduler early. Pass
/// `FlushSchedulerConfig::dirty_threshold_bytes` so the scheduler
/// and backend agree on the trigger point; tests pass `u64::MAX` to
/// disable threshold-driven notifies.
pub async fn attach_manifest(
    disk_manifest_ref: engram_core::types::manifest::ManifestRef,
    cache: engram_chunk_store::cache::ChunkCache,
    store: Arc<engram_chunk_store::ChunkStore>,
    slot_pool: &Arc<NbdSlotAllocator>,
    threshold_bytes: u64,
) -> Result<NbdSandboxState, NbdRuntimeError> {
    let backend =
        ChunkedDiskBackend::from_blob(disk_manifest_ref, cache, store, threshold_bytes).await?;
    attach_backend(backend, slot_pool).await
}

/// ADR 0045 C1: attach from manifest CONTENT delivered inline (a
/// migration destination's not-yet-durable disk manifest).
pub async fn attach_manifest_content(
    disk_manifest_ref: engram_core::types::manifest::ManifestRef,
    manifest: &engram_chunk_store::Manifest,
    cache: engram_chunk_store::cache::ChunkCache,
    store: Arc<engram_chunk_store::ChunkStore>,
    slot_pool: &Arc<NbdSlotAllocator>,
    threshold_bytes: u64,
) -> Result<NbdSandboxState, NbdRuntimeError> {
    let backend = ChunkedDiskBackend::from_manifest(
        disk_manifest_ref,
        manifest,
        cache,
        store,
        threshold_bytes,
    )?;
    attach_backend(backend, slot_pool).await
}

async fn attach_backend(
    backend: ChunkedDiskBackend,
    slot_pool: &Arc<NbdSlotAllocator>,
) -> Result<NbdSandboxState, NbdRuntimeError> {
    let backend = Arc::new(backend);
    let slot = slot_pool.acquire().await;
    let handle = spawn(backend.clone(), slot.path()).await?;
    Ok(NbdSandboxState {
        scheduler: None,
        backend,
        handle,
        slot,
    })
}

/// Spawn a daemon that serves `backend` as a block device at
/// `nbd_device` (e.g. `/dev/nbd0`). Returns once the kernel-side
/// binding is set up and the serve task is running; reads / writes
/// against the path block until then.
///
/// `block_size` is fixed at [`NBD_BLOCK_SIZE`] (4096). The
/// manifest's `total_bytes` must be a multiple of that — typical
/// ext4 images already are.
pub async fn spawn(
    backend: Arc<ChunkedDiskBackend>,
    nbd_device: &Path,
) -> Result<NbdHandle, NbdRuntimeError> {
    let total_bytes = backend.total_bytes();
    if !total_bytes.is_multiple_of(NBD_BLOCK_SIZE) {
        return Err(NbdRuntimeError::UnalignedSize {
            total_bytes,
            block_size: NBD_BLOCK_SIZE,
        });
    }

    // 1. Open the NBD device (O_RDWR). The kernel must already have
    //    the nbd module loaded with enough slots (typically via
    //    `modprobe nbd nbds_max=64`); the host-agent's Packer
    //    manifest handles that.
    let nbd_fd = OwnedFd::from(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(nbd_device)?,
    );

    // 2. socketpair(AF_UNIX, SOCK_STREAM). Both halves are SOCK_STREAM
    //    so reads block until enough bytes arrive (vs SOCK_DGRAM which
    //    would frame-truncate). One end goes to the kernel, the other
    //    stays in-process as a Tokio stream.
    let (kernel_side, server_side) = unix_socketpair()?;

    // 3. Size the block device. The kernel rejects the SET_SOCK ioctl
    //    if these haven't been called.
    let block_count = total_bytes / NBD_BLOCK_SIZE;
    ioctl_set(nbd_fd.as_raw_fd(), NBD_SET_BLKSIZE, NBD_BLOCK_SIZE)?;
    ioctl_set(nbd_fd.as_raw_fd(), NBD_SET_SIZE_BLOCKS, block_count)?;
    let flags = (NBD_FLAG_HAS_FLAGS | NBD_FLAG_SEND_FLUSH | NBD_FLAG_SEND_TRIM) as u64;
    ioctl_set(nbd_fd.as_raw_fd(), NBD_SET_FLAGS, flags)?;

    // 3b. Bound the kernel's per-request wait (see `nbd_kernel_timeout_secs`).
    //     Without NBD_SET_TIMEOUT a daemon that never replies — a wedged
    //     serve loop, a lock deadlock, the daemon dying — leaves guest
    //     I/O (notably the device-open / partition-probe read at attach)
    //     wedged in D-state indefinitely; with it the kernel times the
    //     request out and returns EIO. Takes the timeout in seconds.
    ioctl_set(
        nbd_fd.as_raw_fd(),
        NBD_SET_TIMEOUT,
        nbd_kernel_timeout_secs(),
    )?;

    // 4. Hand the kernel its half of the socketpair. After this,
    //    the kernel speaks NBD wire protocol over its end; we
    //    serve from ours.
    //
    //    SAFETY: kernel_side is a valid OwnedFd we own; ioctl
    //    NBD_SET_SOCK takes the fd value and the kernel duplicates
    //    it internally. We drop our handle to kernel_side
    //    immediately after — the kernel keeps it alive via its
    //    internal reference.
    ioctl_set(
        nbd_fd.as_raw_fd(),
        NBD_SET_SOCK,
        kernel_side.as_raw_fd() as u64,
    )?;
    drop(kernel_side);

    // 5. Spawn the kernel-blocked thread. `NBD_DO_IT` blocks until
    //    NBD_DISCONNECT is issued; without this thread, the kernel
    //    won't process I/O on the block device.
    let nbd_fd_raw = nbd_fd.as_raw_fd();
    let device_label = nbd_device.display().to_string();
    let kernel_thread = std::thread::Builder::new()
        .name(format!("engram-nbd-doit-{}", device_label))
        .spawn(move || {
            // SAFETY: nbd_fd_raw is alive for the duration of this
            // thread because the parent NbdHandle owns the OwnedFd
            // and Drop joins us before closing.
            let rc = unsafe { libc::ioctl(nbd_fd_raw, NBD_DO_IT) };
            if rc != 0 {
                let errno = io::Error::last_os_error().raw_os_error();
                tracing::warn!(rc, errno, "NBD_DO_IT exited unexpectedly");
            }
        })
        .map_err(io::Error::other)?;

    // 6. Spawn the tokio serve task on the server-side socket.
    let stream = TokioUnixStream::from_std(server_side)?;
    let serve_task = tokio::spawn(serve_loop(backend, stream));

    Ok(NbdHandle {
        nbd_device: nbd_device.to_path_buf(),
        nbd_fd: Some(nbd_fd),
        serve_task: Some(serve_task),
        kernel_thread: Some(kernel_thread),
    })
}

/// Run an NBD ioctl that takes a u64 argument. The kernel reads
/// the argument as a `unsigned long`, so we pass it as `u64` and
/// `libc::ioctl` handles the platform-specific width.
fn ioctl_set(fd: RawFd, cmd: libc::Ioctl, arg: u64) -> io::Result<()> {
    // SAFETY: fd is an owned, valid kernel fd handed in by the
    // caller. NBD ioctl numbers don't carry direction bits — the
    // kernel reads `arg` as `unsigned long`, which is u64 on
    // x86_64 / aarch64. Calling convention matches.
    let rc = unsafe { libc::ioctl(fd, cmd, arg) };
    if rc != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `socketpair(AF_UNIX, SOCK_STREAM)` returning `(kernel_side,
/// server_side)`. Both are `OwnedFd` so dropping closes cleanly.
fn unix_socketpair() -> io::Result<(OwnedFd, std::os::unix::net::UnixStream)> {
    let mut fds = [0i32; 2];
    // SAFETY: array sized for the AF_UNIX socketpair contract.
    // Kernel writes both fds; we wrap them in OwnedFd /
    // UnixStream immediately to take ownership.
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: socketpair guarantees both fds are valid.
    let kernel_side = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let server_std = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fds[1]) };
    server_std.set_nonblocking(true)?;
    Ok((kernel_side, server_std))
}

/// Serve NBD requests on `stream` until the kernel disconnects or
/// the stream errors. Each request: read 28-byte header → parse →
/// dispatch to backend → write reply (16-byte header + optional
/// payload). Sequential per session — kernel-side I/O concurrency
/// is handled by the kernel, our serve loop just needs to keep up.
async fn serve_loop(backend: Arc<ChunkedDiskBackend>, mut stream: TokioUnixStream) {
    // ADR 0018 commit 12m: hold an `Arc<InFlightTracker>` clone for
    // the lifetime of this connection. Each request handler scope
    // grabs a guard (++count); drop on scope exit decrements and,
    // on the 1→0 edge, wakes any `wait_idle()` parker. The snapshot
    // pipeline calls `backend.wait_idle().await` after `inner.pause()`
    // to drain the virtio→kernel-NBD→userspace pipeline before
    // flushing.
    let in_flight = backend.in_flight_tracker();
    loop {
        let mut header = [0u8; REQUEST_HEADER_LEN];
        match stream.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                tracing::info!("NBD serve loop: kernel closed socket cleanly");
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "NBD serve loop: read header failed");
                return;
            }
        }
        let req = match NbdRequest::parse(&header) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "NBD serve loop: malformed request header");
                return;
            }
        };

        // Disconnect short-circuits before we record an in-flight —
        // it's just a sentinel to break the loop, nothing the
        // barrier needs to wait on.
        if matches!(req.command, NbdCommand::Disconnect) {
            tracing::info!("NBD client requested disconnect");
            return;
        }
        let _guard = in_flight.enter();

        match req.command {
            NbdCommand::Read => {
                // `backend.read` self-bounds via the chunk-fetch retry
                // budget (per-attempt timeout × max attempts), so a
                // stalled/missing chunk surfaces as an `Err` → EIO in
                // bounded time rather than hanging. The kernel-side
                // NBD_SET_TIMEOUT (see `spawn`) is the backstop for the
                // cases the budget can't cover (a wedged serve loop or
                // lock — where the daemon never replies at all).
                let bytes = match backend.read(req.offset, req.length as u64).await {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(error = %e, "NBD read failed");
                        let _ = stream.write_all(&NbdReply::eio(req.handle).encode()).await;
                        continue;
                    }
                };
                let reply = NbdReply::ok(req.handle);
                if let Err(e) = stream.write_all(&reply.encode()).await {
                    tracing::warn!(error = %e, "NBD reply header write failed");
                    return;
                }
                if let Err(e) = stream.write_all(&bytes).await {
                    tracing::warn!(error = %e, "NBD reply payload write failed");
                    return;
                }
            }
            NbdCommand::Write => {
                let mut data = vec![0u8; req.length as usize];
                if let Err(e) = stream.read_exact(&mut data).await {
                    tracing::warn!(error = %e, "NBD write payload read failed");
                    return;
                }
                let reply = match backend.write(req.offset, &data).await {
                    Ok(()) => NbdReply::ok(req.handle),
                    Err(e) => {
                        tracing::warn!(error = %e, "NBD write failed");
                        NbdReply::eio(req.handle)
                    }
                };
                if let Err(e) = stream.write_all(&reply.encode()).await {
                    tracing::warn!(error = %e, "NBD write reply failed");
                    return;
                }
            }
            NbdCommand::Disconnect => {
                // Handled above before `in_flight.enter()` to avoid
                // recording a phantom in-flight on the tear-down
                // request. This arm is unreachable in practice.
                unreachable!("Disconnect handled before the in-flight guard")
            }
            NbdCommand::Flush => {
                // Honour the FLUSH semantic at the wire level
                // (ack immediately) but defer durable-flush to the
                // snapshot path. Inline flush-to-chunk-store per
                // FUA / FLUSH would multiply object-storage cost
                // by 100x for a typical workload; we trade off
                // strict-FUA for cost.
                let _ = stream.write_all(&NbdReply::ok(req.handle).encode()).await;
            }
            NbdCommand::Trim => {
                // Same trade-off as FLUSH — accept the request
                // (so the kernel doesn't mark the device as
                // unsupporting trim) but treat as a no-op. A
                // proper implementation would mark the affected
                // chunks as "zero-fill on next read"; deferred.
                let _ = stream.write_all(&NbdReply::ok(req.handle).encode()).await;
            }
        }
    }
}
