//! Linux NBD server loop + kernel `/dev/nbdN` orchestration.
//!
//! The host-agent calls [`spawn`] with an allocated `/dev/nbdN`
//! path and a [`ChunkedDiskBackend`]. The function:
//!
//! 1. Creates a `socketpair(AF_UNIX, SOCK_STREAM)` — one end goes
//!    to the kernel, the other stays in-process so the daemon can
//!    serve requests over it.
//! 2. Configures the device via the NBD **netlink** interface
//!    (`NBD_CMD_CONNECT` with size / block-size / flags / timeouts
//!    / the kernel-side socket fd). Netlink, not the legacy
//!    `NBD_SET_SOCK`+`NBD_DO_IT` ioctls: the ioctl mode welds the
//!    device's data plane to a thread of THIS process, so a
//!    host-agent pod roll killed the disk under every surviving FC
//!    VM and could wedge the slot until reboot (prod 2026-06-11,
//!    /dev/nbd4). In netlink mode the kernel runs its own receive
//!    machinery, `NBD_ATTR_DEAD_CONN_TIMEOUT` parks guest I/O while
//!    no server is connected, and [`reattach`] hands the kernel a
//!    fresh socket via `NBD_CMD_RECONFIGURE` after a restart — the
//!    survivor-rehydrate primitive.
//! 3. Spawns a tokio task that reads NBD requests over the
//!    server-side `UnixStream`, dispatches to the backend, and
//!    writes replies. Each request is served sequentially —
//!    in-flight pipelining is a follow-up optimization (the kernel
//!    side handles many handles concurrently but a sequential
//!    serve is correct).
//!
//! Shutdown: drop the returned [`NbdHandle`] to tear down. The
//! `Drop` impl aborts the serve task and issues a netlink
//! `NBD_CMD_DISCONNECT` from a detached thread (no fd, no blocked
//! `NBD_DO_IT` thread to join — that whole failure family is gone).
//!
//! `unsafe` blocks are the unavoidable kernel-syscall surface
//! (raw `libc::socketpair`, fd ownership transfer). Each is
//! annotated.

#![cfg(target_os = "linux")]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream as TokioUnixStream;
use tokio::task::JoinHandle as TokioJoinHandle;

use super::backend::{ChunkedDiskBackend, DiskBackendError};
use super::nbd::{NbdCommand, NbdReply, NbdRequest, REQUEST_HEADER_LEN};
use super::nbd_netlink::{self, NbdNetlinkParams};
use super::slot::{NbdSlot, NbdSlotAllocator};

/// Block size the daemon hard-pins. 4096 matches the kernel's
/// page size on x86_64 and the chunk-aligned units we serve.
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

/// `NBD_ATTR_DEAD_CONN_TIMEOUT` in seconds: how long guest I/O is
/// PARKED (requeued, not failed) while the device has no live server
/// connection. This is the pod-roll grace window — the old host-agent
/// dies with the serve socket, the new one comes up, registers,
/// learns its survivors, and [`reattach`]es a fresh socket; the guest
/// rides the gap in D-state instead of taking EIO + an errored ext4.
/// Sized to cover restart + registration + rehydrate with slack.
/// Override via `ENGRAM_NBD_DEAD_CONN_TIMEOUT_SECS`.
fn nbd_dead_conn_timeout_secs() -> u64 {
    std::env::var("ENGRAM_NBD_DEAD_CONN_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(300)
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

/// Live NBD daemon handle. Holds the spawned tokio serve task and
/// the device's netlink identity. Dropping cleanly tears everything
/// down. Deliberately NO open fd to the device and NO kernel-blocked
/// thread: the netlink configuration lives in the kernel, decoupled
/// from this process — that decoupling is what lets a surviving FC's
/// disk outlive a host-agent restart.
pub struct NbdHandle {
    nbd_device: PathBuf,
    /// Device minor (`N` of `/dev/nbdN`) for netlink commands.
    index: u32,
    /// Background tokio task running the NBD serve loop. Aborted
    /// on `Drop`.
    serve_task: Option<TokioJoinHandle<()>>,
}

impl NbdHandle {
    /// The path the daemon is bound to. Pass this as FC's
    /// `path_on_host` for the rootfs drive.
    pub fn device_path(&self) -> &Path {
        &self.nbd_device
    }

    /// Kill the serve loop WITHOUT disconnecting the kernel config —
    /// the device stays configured with a dead connection, exactly
    /// what the kernel observes when the whole host-agent process
    /// dies (pod roll). Guest I/O then parks under
    /// `dead_conn_timeout` until a successor [`reattach`]es. Test
    /// support for the survivor-rehydrate path; production death is
    /// the real thing.
    pub fn abandon(mut self) {
        if let Some(task) = self.serve_task.take() {
            task.abort();
        }
        // Skip Drop (which would netlink-disconnect).
        std::mem::forget(self);
    }
}

impl Drop for NbdHandle {
    fn drop(&mut self) {
        // Abort the serve loop first (its socket half dying is what
        // the kernel's recv worker observes), then issue the netlink
        // disconnect from a detached thread: the genl round-trip is
        // normally instant, but it can wait on in-flight kernel-side
        // teardown and Drop often runs on a tokio worker (ADR 0017
        // Phase A taught us not to block those — four blocked
        // destroys once stalled the whole runtime). No fd to close
        // and no NBD_DO_IT thread to join in netlink mode; if the
        // disconnect errors, the slot allocator's
        // /sys/block/nbdN/pid probe keeps the device structurally
        // invisible until the startup recovery (or reboot) frees it.
        if let Some(task) = self.serve_task.take() {
            task.abort();
        }
        let device_path = self.nbd_device.clone();
        let index = self.index;
        std::thread::Builder::new()
            .name(format!("nbd-detach-{index}"))
            .spawn(move || match nbd_netlink::disconnect_device(index) {
                Ok(()) => tracing::debug!(
                    device = %device_path.display(),
                    "NBD netlink disconnect complete",
                ),
                Err(e) => tracing::warn!(
                    device = %device_path.display(),
                    error = %e,
                    "NBD netlink disconnect failed during shutdown",
                ),
            })
            .ok(); // best-effort; if spawn fails the device stays busy until startup recovery
    }
}

/// ADR 0017 Phase B: probe each device in `paths` for stale
/// kernel-side bindings (a populated `/sys/block/nbdN/pid` pointing
/// at a process that's no longer alive — the usual aftermath of an
/// ungraceful host-agent exit). For each, issue a netlink
/// `NBD_CMD_DISCONNECT` to make the kernel release it. Returns
/// `(probed, recovered, still_stuck)`.
///
/// Best-effort: a recovery that doesn't clear the pid file is
/// logged with `tracing::warn!` so prod ops sees how many devices
/// can't be recovered automatically (operator fallback: reboot the
/// host). The pool-acquire path's `nbd_kernel_busy` probe will
/// continue to skip still-stuck devices, so they're structurally
/// invisible until the kernel releases.
///
/// MUST run only over slots that do NOT belong to surviving
/// sandboxes — a survivor's device is alive-by-design across the
/// restart (its FC keeps reading it; rehydrate RECONFIGUREs it).
/// The pre-netlink version of this pass ran blind over every
/// device at startup and actively disconnected the survivor's disk
/// (prod 2026-06-11, /dev/nbd4 → guest rootfs EIO). The caller in
/// `lib.rs` therefore runs it AFTER the survivor rehydrate pass,
/// over the slot pool's still-free paths only.
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
        "NBD recovery: unclaimed device kernel-bound to a stale config; \
         attempting recovery via netlink NBD_CMD_DISCONNECT",
    );

    // 2. Netlink disconnect. Works without an open fd and without
    //    the dead process's NBD_DO_IT thread — this is what the old
    //    ioctl dance (NBD_DISCONNECT + NBD_CLEAR_SOCK on a fresh fd)
    //    couldn't reliably do ("device STILL bound", prod
    //    2026-06-11). Errors are folded into the re-probe below.
    let index = nbd_netlink::device_index(path)?;
    if let Err(e) = nbd_netlink::disconnect_device(index) {
        tracing::debug!(
            device = %path.display(),
            error = %e,
            "netlink NBD_CMD_DISCONNECT in recovery errored; re-probing anyway",
        );
    }

    // 3. Brief sleep so the kernel has a chance to release, then
    //    re-probe. Empty → recovered; non-empty → still stuck.
    std::thread::sleep(std::time::Duration::from_millis(100));
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
            "NBD recovery: device STILL bound after netlink disconnect; \
             operator may need to reboot the host to recover this slot",
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

    /// Graceful-shutdown teardown that leaves the KERNEL side alive
    /// for the successor host-agent generation (ADR 0044 K2). Without
    /// this, process exit drops [`NbdHandle`] → netlink disconnect →
    /// the survivor's disk is torn down by its own dying parent
    /// ("Disconnected due to user request", prod 2026-06-12 canary)
    /// and the successor's RECONFIGURE meets "not configured". The
    /// serve task + scheduler are aborted (in-process resources); the
    /// device config, with its parked-I/O dead_conn window, persists.
    /// The slot lease is forgotten rather than released — the pool
    /// dies with the process, and `NbdSlot::Drop` would
    /// `tokio::spawn` during runtime teardown.
    pub fn abandon_for_shutdown(self) {
        drop(self.scheduler);
        self.handle.abandon();
        std::mem::forget(self.slot);
        drop(self.backend);
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
    let backend_id = disk_manifest_ref.manifest_id.to_string();
    let backend =
        ChunkedDiskBackend::from_blob(disk_manifest_ref, cache, store, threshold_bytes).await?;
    attach_backend(backend, slot_pool, &backend_id).await
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
    let backend_id = disk_manifest_ref.manifest_id.to_string();
    let backend = ChunkedDiskBackend::from_manifest(
        disk_manifest_ref,
        manifest,
        cache,
        store,
        threshold_bytes,
    )?;
    attach_backend(backend, slot_pool, &backend_id).await
}

/// Survivor rehydrate (ADR 0044 K2): rebuild the data plane for a
/// device the kernel ALREADY serves under a surviving FC. The slot
/// must have been [`NbdSlotAllocator::claim`]ed for the survivor's
/// existing `/dev/nbdN`; a fresh backend is built from the durable
/// manifest and handed to the kernel via netlink
/// `NBD_CMD_RECONFIGURE` — the kernel swaps the dead pod's socket
/// for ours and releases any guest I/O parked under
/// `dead_conn_timeout`. (The pre-netlink rehydrate acquired a FRESH
/// slot here, serving a device nobody read while the survivor's
/// real device stayed dead.)
pub async fn reattach_manifest(
    disk_manifest_ref: engram_core::types::manifest::ManifestRef,
    cache: engram_chunk_store::cache::ChunkCache,
    store: Arc<engram_chunk_store::ChunkStore>,
    slot: NbdSlot,
    threshold_bytes: u64,
) -> Result<NbdSandboxState, NbdRuntimeError> {
    let backend_id = disk_manifest_ref.manifest_id.to_string();
    let backend = Arc::new(
        ChunkedDiskBackend::from_blob(disk_manifest_ref, cache, store, threshold_bytes).await?,
    );
    let handle = reattach(backend.clone(), slot.path(), &backend_id).await?;
    Ok(NbdSandboxState {
        scheduler: None,
        backend,
        handle,
        slot,
    })
}

async fn attach_backend(
    backend: ChunkedDiskBackend,
    slot_pool: &Arc<NbdSlotAllocator>,
    backend_id: &str,
) -> Result<NbdSandboxState, NbdRuntimeError> {
    let backend = Arc::new(backend);
    let slot = slot_pool.acquire().await;
    let handle = spawn(backend.clone(), slot.path(), backend_id).await?;
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
    backend_id: &str,
) -> Result<NbdHandle, NbdRuntimeError> {
    serve_at(backend, nbd_device, backend_id, ConnectMode::Connect).await
}

/// Hand the kernel a NEW serve socket for a device it already has
/// configured (netlink `NBD_CMD_RECONFIGURE`) — the survivor-
/// rehydrate primitive. `backend_id` must match the identifier the
/// original CONNECT registered (the kernel verifies it via
/// `/sys/block/nbdN/backend`, so a slot-accounting bug can't splice
/// our socket into someone else's device).
///
/// ADOPTION IS VERIFIED, NOT TRUSTED: the kernel's reconfigure
/// handler converts `-ENOSPC` ("no dead connection slot to replace
/// yet") into a clean ACK and quietly drops the socket — observed
/// live when the predecessor's serve fd lingered in a child and the
/// dead-marking only happened at the next request timeout. A
/// dropped socket EOFs our serve loop within milliseconds (the
/// kernel `sockfd_put`s its only reference), so after each attempt
/// we wait briefly and check the serve task is still alive,
/// retrying with a fresh socketpair until the kernel has actually
/// marked the old connection dead. Budget covers a full 90s request
/// timeout straggler.
pub async fn reattach(
    backend: Arc<ChunkedDiskBackend>,
    nbd_device: &Path,
    backend_id: &str,
) -> Result<NbdHandle, NbdRuntimeError> {
    let budget = std::time::Duration::from_secs(150);
    let started = std::time::Instant::now();
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let handle = serve_at(
            backend.clone(),
            nbd_device,
            backend_id,
            ConnectMode::Reconfigure,
        )
        .await?;
        // An adopted socket stays open (the kernel holds its dup);
        // a rejected one EOFs the serve loop near-instantly.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        if !handle
            .serve_task
            .as_ref()
            .map(|t| t.is_finished())
            .unwrap_or(true)
        {
            if attempt > 1 {
                tracing::info!(
                    device = %nbd_device.display(),
                    attempt,
                    waited_ms = started.elapsed().as_millis() as u64,
                    "NBD RECONFIGURE adopted after retries",
                );
            }
            return Ok(handle);
        }
        // Not adopted. Drop WITHOUT the netlink disconnect (the
        // device must stay configured for the next attempt — and
        // for the parked guest I/O).
        handle.abandon();
        if started.elapsed() > budget {
            return Err(NbdRuntimeError::Io(io::Error::other(format!(
                "NBD RECONFIGURE not adopted within {budget:?} ({attempt} attempts): \
                 the kernel reports success but closes the socket — predecessor's \
                 connection never marked dead?"
            ))));
        }
        tracing::debug!(
            device = %nbd_device.display(),
            attempt,
            "NBD RECONFIGURE socket not adopted (kernel-swallowed ENOSPC); retrying",
        );
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

enum ConnectMode {
    Connect,
    Reconfigure,
}

async fn serve_at(
    backend: Arc<ChunkedDiskBackend>,
    nbd_device: &Path,
    backend_id: &str,
    mode: ConnectMode,
) -> Result<NbdHandle, NbdRuntimeError> {
    let total_bytes = backend.total_bytes();
    if !total_bytes.is_multiple_of(NBD_BLOCK_SIZE) {
        return Err(NbdRuntimeError::UnalignedSize {
            total_bytes,
            block_size: NBD_BLOCK_SIZE,
        });
    }
    let index = nbd_netlink::device_index(nbd_device)?;

    // 1. socketpair(AF_UNIX, SOCK_STREAM). Both halves are SOCK_STREAM
    //    so reads block until enough bytes arrive (vs SOCK_DGRAM which
    //    would frame-truncate). One end goes to the kernel, the other
    //    stays in-process as a Tokio stream.
    let (kernel_side, server_side) = unix_socketpair()?;

    // 2. Configure (or re-arm) the device via netlink. The kernel
    //    dups the socket fd, runs its own receive machinery (no
    //    NBD_DO_IT thread), and parks guest I/O for
    //    `dead_conn_timeout` whenever the connection dies — the
    //    pod-roll survival contract. The genl round-trips are
    //    blocking syscalls with a bounded recv timeout; run them off
    //    the async workers.
    let params_fd = kernel_side.as_raw_fd();
    let backend_id_owned = backend_id.to_string();
    let connect = tokio::task::spawn_blocking(move || {
        let params = NbdNetlinkParams {
            index,
            sock_fd: params_fd,
            timeout_secs: nbd_kernel_timeout_secs(),
            dead_conn_timeout_secs: nbd_dead_conn_timeout_secs(),
            backend_identifier: &backend_id_owned,
        };
        match mode {
            ConnectMode::Connect => {
                nbd_netlink::connect_device(&params, total_bytes, NBD_BLOCK_SIZE)
            }
            ConnectMode::Reconfigure => nbd_netlink::reconfigure_device(&params),
        }
    })
    .await
    .map_err(io::Error::other)?;
    connect?;
    // The kernel holds its own reference now.
    drop(kernel_side);

    // 3. Spawn the tokio serve task on the server-side socket.
    let stream = TokioUnixStream::from_std(server_side)?;
    let serve_task = tokio::spawn(serve_loop(backend, stream));

    Ok(NbdHandle {
        nbd_device: nbd_device.to_path_buf(),
        index,
        serve_task: Some(serve_task),
    })
}

/// `socketpair(AF_UNIX, SOCK_STREAM)` returning `(kernel_side,
/// server_side)`. Both are `OwnedFd` so dropping closes cleanly.
fn unix_socketpair() -> io::Result<(OwnedFd, std::os::unix::net::UnixStream)> {
    let mut fds = [0i32; 2];
    // SAFETY: array sized for the AF_UNIX socketpair contract.
    // Kernel writes both fds; we wrap them in OwnedFd /
    // UnixStream immediately to take ownership.
    //
    // SOCK_CLOEXEC is LOAD-BEARING for the survivor contract: these
    // fds are created with raw libc (no CLOEXEC by default), so
    // every child spawned afterwards — Firecracker above all —
    // inherited the server half. The child then kept the socket
    // open past the host-agent's death, the kernel's recv worker
    // never saw EOF, the dead nsock was only marked at the next
    // 90s request timeout, and the successor's RECONFIGURE within
    // that window met the kernel's silently-ACKed -ENOSPC ("no
    // dead connection to replace") — prod canary 2026-06-12,
    // "Receive control failed (result -32)" arriving ~90s late.
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
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
