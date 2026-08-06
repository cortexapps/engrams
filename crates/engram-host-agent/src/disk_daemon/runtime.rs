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
//!    server-side `UnixStream` and PIPELINES them (ADR 0071): a
//!    reader dispatches each request to a bounded pool of handler
//!    tasks and a single writer serializes the replies back. The
//!    kernel issues many in-flight requests per socket (correlated
//!    by `handle`); servicing them concurrently overlaps the
//!    per-request chunk fetches. `ENGRAM_NBD_SERVE_CONCURRENCY=1`
//!    falls back to the strictly-serial loop.
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

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream as TokioUnixStream;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle as TokioJoinHandle;

use engram_host_core::{NbdConnectRequest, NbdKernel, NbdReconfigureRequest};

use super::backend::{ChunkedDiskBackend, DirtyFileOpenMode, DiskBackendError, InFlightGuard};
use super::nbd::{
    NbdCommand, NbdReply, NbdRequest, MAX_REQUEST_PAYLOAD_BYTES, REPLY_HEADER_LEN,
    REQUEST_HEADER_LEN,
};
use super::nbd_kernel::HostNbdKernel;
use super::nbd_netlink;
use super::slot::{NbdSlot, NbdSlotAllocator};

/// Block size the daemon hard-pins. 4096 matches the kernel's
/// page size on x86_64 and the chunk-aligned units we serve.
pub const NBD_BLOCK_SIZE: u64 = 4096;

/// Terminal shutdown-abandon mode (2026-08-02 durability-rollback RCA).
/// Raised once at SIGTERM — BEFORE the background-task aborts, whose
/// dropped locals can hold a live [`NbdHandle`] — and never lowered.
/// While raised, [`NbdHandle`]'s `Drop` leaves the kernel device
/// configured instead of netlink-disconnecting it: in the SIGTERM window
/// a handle only reaches `Drop` via unwind or teardown collateral (the
/// abandon sweep uses [`NbdHandle::abandon`], which skips `Drop`), and a
/// disconnect from that path de-configures a surviving guest's live
/// device — the successor's RECONFIGURE then meets "not configured" and
/// the survivor becomes an uncapturable quarantine. The 2026-08-02 panic
/// between the flush pass and the abandon sweep disconnected four
/// survivors exactly this way. The decision itself is the pure
/// [`engram_host_core::nbd_drop_action`]; this flag is its process-global
/// input.
static SHUTDOWN_ABANDON_MODE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Raise the terminal shutdown-abandon mode. Called from the SIGTERM
/// handler as its first act; there is deliberately no way to lower it.
pub fn enter_shutdown_abandon_mode() {
    SHUTDOWN_ABANDON_MODE.store(true, std::sync::atomic::Ordering::SeqCst);
}

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

/// Max NBD requests serviced concurrently per sandbox (ADR 0071). The
/// kernel issues many in-flight requests on one socket (each carries a
/// `handle` for correlation); this bounds how many the daemon services
/// at once, overlapping the per-request chunk fetches instead of
/// serializing them. `1` reproduces the legacy strictly-serial serve
/// loop exactly — the kill-switch. Override via
/// `ENGRAM_NBD_SERVE_CONCURRENCY`.
fn nbd_serve_concurrency() -> usize {
    std::env::var("ENGRAM_NBD_SERVE_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(8)
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
        // 2026-08-02 durability-rollback RCA: once shutdown is underway,
        // a drop is unwind/teardown collateral, never a deliberate
        // survivor teardown — leave the kernel config alive so the
        // successor's RECONFIGURE finds a configured device (see
        // `SHUTDOWN_ABANDON_MODE`).
        match engram_host_core::nbd_drop_action(
            SHUTDOWN_ABANDON_MODE.load(std::sync::atomic::Ordering::SeqCst),
        ) {
            engram_host_core::NbdDropAction::LeaveKernelConfigured => {
                tracing::warn!(
                    device = %self.nbd_device.display(),
                    "NbdHandle dropped during shutdown-abandon mode; kernel \
                     config left alive for the successor (no netlink \
                     disconnect) — this drop bypassed the abandon sweep, \
                     likely a panic unwind",
                );
                return;
            }
            engram_host_core::NbdDropAction::Disconnect => {}
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
///
/// CLAIM-THEN-DISCONNECT (TOCTOU close): the candidate list is a
/// point-in-time snapshot of free paths, but the gRPC server is
/// already up and a concurrent `create()` can `acquire` + CONNECT a
/// device that was free at snapshot time. Because this pass sleeps
/// 100 ms per device it runs hundreds of ms behind the snapshot, so a
/// racing session can win the slot first. We therefore `try_claim`
/// each candidate FROM THE POOL before touching it: if the claim fails
/// someone owns it now (a fresh session, a survivor) and we skip; if it
/// succeeds we hold the reserved bit across the (slow) DISCONNECT so no
/// one can be handed the device mid-sweep, then release it back. The
/// per-device pid-liveness/self-pid guard in [`recover_one_stuck_device`]
/// is the defense-in-depth second line.
pub async fn recover_stuck_nbd_devices(
    pool: &Arc<NbdSlotAllocator>,
    reap: engram_host_core::ReapList<std::path::PathBuf>,
) -> (usize, usize, usize) {
    let mut probed = 0;
    let mut recovered = 0;
    let mut still_stuck = 0;
    let mut parked = 0;
    // The ordering contract, enforced by the TYPE: this destructive pass accepts
    // ONLY a `ReapList` — the [`SlotClass::TerminalSafeToReap`] subset the
    // startup classification barrier produced (ADR 0098 §Phase 3, Wave 7b, #784
    // layer 3). It is unconstructible without having classified, so a device
    // reaches the DISCONNECT below iff classification proved it a genuine stale
    // binding. The per-device liveness/holder re-check inside
    // `recover_one_stuck_device` remains as defense-in-depth against the
    // classify→disconnect TOCTOU (a device that gained a live holder since
    // classification re-PARKs).
    for path in reap.devices() {
        // Reserve the slot before probing/disconnecting. A failed claim
        // means a concurrent acquire/claim already owns it — by
        // definition not a stale binding, so skip it entirely.
        let Some(slot) = pool.try_claim(path).await else {
            tracing::debug!(
                device = %path.display(),
                "NBD recovery: device claimed by another lease since the free-paths \
                 snapshot; skipping (not stale)",
            );
            continue;
        };
        // The blocking probe (DISCONNECT + 100ms sleep + re-probe) runs
        // on a blocking thread; we keep `slot` reserved for its duration
        // and release it (via Drop) right after.
        let probe_path = path.clone();
        let outcome =
            tokio::task::spawn_blocking(move || recover_one_stuck_device(&probe_path)).await;
        // Drop the lease → release the slot back to the pool for the
        // populator to re-validate (a still-stuck device fails the
        // free-check and is skipped; a recovered one re-warms).
        drop(slot);
        match outcome {
            Ok(Ok(NbdRecoveryOutcome::NotStuck)) => {}
            Ok(Ok(NbdRecoveryOutcome::Recovered)) => {
                probed += 1;
                recovered += 1;
            }
            Ok(Ok(NbdRecoveryOutcome::StillStuck)) => {
                probed += 1;
                still_stuck += 1;
            }
            Ok(Ok(NbdRecoveryOutcome::Parked)) => {
                // R6 (#769 gap A): a live holder blocked the disconnect. The
                // slot was dropped back to the pool above, but the device stays
                // kernel-bound (we did NOT disconnect), so the populator's
                // `nbd_kernel_busy` validation keeps it out of new-claim
                // circulation while a later rehydrate re-serves it by path. The
                // soft-invariant + metric already fired inside
                // `recover_one_stuck_device`.
                probed += 1;
                parked += 1;
            }
            Ok(Err(e)) => {
                probed += 1;
                still_stuck += 1;
                tracing::debug!(
                    device = %path.display(),
                    error = %e,
                    "NBD recovery: error during probe; treating as still-stuck",
                );
            }
            Err(e) => {
                probed += 1;
                still_stuck += 1;
                tracing::warn!(
                    device = %path.display(),
                    error = %e,
                    "NBD recovery: probe task panicked/cancelled; treating as still-stuck",
                );
            }
        }
    }
    if probed > 0 {
        tracing::warn!(
            probed,
            recovered,
            still_stuck,
            parked,
            "NBD startup cleanup: recovered {recovered} stale NBD devices (of {probed} probed; \
             {still_stuck} still stuck; {parked} parked — live holder, left reconnectable)",
        );
    }
    (probed, recovered, still_stuck)
}

enum NbdRecoveryOutcome {
    NotStuck,
    Recovered,
    StillStuck,
    /// R6 (#769 gap A): the dead-owner binding is left RECONNECTABLE — a live
    /// process still holds the device node open (or the holder scan was
    /// inconclusive), so DISCONNECTing would sever a surviving guest. The
    /// device stays kernel-bound (structurally invisible to new claims via the
    /// `nbd_kernel_busy` probe) for a later rehydrate/re-serve pass to adopt.
    Parked,
}

/// Scan `/proc/*/fd/*` for any live process holding an open fd on `device`
/// (`/dev/nbdN`) — the R6 proof-of-death probe (ADR 0098 §Phase 3, #784 layer
/// 1 / #769 gap A). The stale-binding sweep must never DISCONNECT a device a
/// surviving guest is still reading, even when the configuring server pid is
/// dead: the host-side FC process opens the NBD device node directly (its
/// virtio-blk rootfs drive source), so a live guest shows up here as an fd
/// readlink resolving to the device node.
///
/// **Cold path only** — called at most once per FREE candidate device during
/// the startup stale-binding sweep (never on the request path). Cost is a
/// single `/proc` readdir plus a bounded per-process fd readdir, so it is
/// O(processes × open-fds) on the host, paid once at boot.
///
/// Fail-safe: returns [`DeviceHolder::Unknown`] whenever the scan cannot rule
/// a holder out — `/proc` unreadable, or ANY process's fd directory
/// unreadable for a reason other than the process having exited (a permission
/// error means we cannot see that process's fds, so it could be the holder).
/// Absence of proof is not proof of death, and the pure verdict PARKs on
/// Unknown exactly as it does on a live holder.
///
/// TRANSIENT SYSTEM HOLDERS are benign here. An NBD connect/change uevent makes
/// systemd-udevd briefly open the block device to probe it (blkid et al.); if
/// that probe overlaps a sweep tick, this returns `LiveHolder` for a device
/// whose guest is actually gone. The cost is bounded and self-healing: the
/// verdict PARKs (leaves the device kernel-bound, RECONNECTABLE — the
/// `nbd_kernel_busy` probe keeps it out of new-claim circulation) instead of
/// disconnecting. udev closes its probe fd within milliseconds, so a subsequent
/// sweep pass (the next host-agent register/roll — the sweep is per-register,
/// not a periodic loop) sees `NoHolder` and disconnects legally; the interim
/// cost is one reconnectable-but-parked slot, never a wrongful disconnect of a
/// live device. This is the intended fail-safe asymmetry: deferring a
/// disconnect is cheap and reversible, severing a device a guest is reading is
/// not.
///
/// `pub` so the FC-lane test (`nbd_proc_holder`) can pin the kernel assumption
/// this guard leans on: an open fd on a real `/dev/nbdN` with a dead netlink
/// server is detectable via the `/proc` scan.
pub fn device_has_live_holder(device: &std::path::Path) -> engram_host_core::DeviceHolder {
    use engram_host_core::DeviceHolder;
    // Canonicalize once so a symlinked /dev entry compares equal to the
    // kernel's readlink result; fall back to the literal path if it can't be
    // resolved (still a valid comparison target).
    let target = std::fs::canonicalize(device).unwrap_or_else(|_| device.to_path_buf());
    let proc = match std::fs::read_dir("/proc") {
        Ok(rd) => rd,
        Err(_) => return DeviceHolder::Unknown,
    };
    let mut inconclusive = false;
    for entry in proc.flatten() {
        // Only numeric (pid) directories carry an fd table.
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let fds = match std::fs::read_dir(entry.path().join("fd")) {
            Ok(fds) => fds,
            Err(e) => {
                // ENOENT: the process exited between the /proc readdir and here
                // — a real negative, skip it. Any other error (EACCES/EPERM)
                // means we cannot see this process's fds, so it could be the
                // holder: the scan is inconclusive → fail safe to Unknown.
                if e.kind() != std::io::ErrorKind::NotFound {
                    inconclusive = true;
                }
                continue;
            }
        };
        for fd in fds.flatten() {
            if let Ok(link) = std::fs::read_link(fd.path()) {
                if link == target || link == *device {
                    return DeviceHolder::LiveHolder;
                }
            }
        }
    }
    if inconclusive {
        DeviceHolder::Unknown
    } else {
        DeviceHolder::NoHolder
    }
}

/// The Layer-2 kernel-derived inventory + Layer-3 classification barrier (ADR
/// 0098 §Phase 3, Wave 7b, #784). Enumerate KERNEL GROUND TRUTH — every
/// CONNECTED `/dev/nbdN` ([`NbdKernel::connected_devices`]) — and RECONCILE the
/// tracked records against it: for each connected device resolve the owner's
/// liveness (self / alive / dead vs this process's pid), the live-holder proof
/// of death (only for a dead owner — the cold `/proc` scan is skipped where the
/// verdict can't depend on it), and whether any tracked record references the
/// device (`record_devices`). [`classify_startup_slots`] then assigns each slot
/// exactly one class.
///
/// The records are matched AGAINST the kernel inventory, never the reverse — a
/// connected device NO record accounts for surfaces as
/// [`SlotClass::QuarantinedUnknown`](engram_host_core::SlotClass::QuarantinedUnknown)
/// (the #769 gap-A survivor), not a silent skip. The returned
/// [`StartupClassification`](engram_host_core::StartupClassification)'s
/// `reap` is the ONLY input the destructive [`recover_stuck_nbd_devices`] sweep
/// accepts (the ordering contract, enforced by the type).
///
/// A cold-path scan run once at register time, after the rehydrate passes have
/// re-served the survivors they cover (those show as self-owned ⇒ `Serving`).
pub fn classify_startup_inventory(
    kernel: &dyn NbdKernel,
    record_devices: &std::collections::HashSet<PathBuf>,
) -> engram_host_core::StartupClassification<PathBuf> {
    let self_pid = std::process::id() as i32;
    let slots = kernel
        .connected_devices()
        .into_iter()
        .map(|dev| {
            let liveness = if dev.owner_pid == self_pid {
                engram_host_core::PidLiveness::SelfPid
            } else if pid_is_alive(dev.owner_pid) {
                engram_host_core::PidLiveness::Alive
            } else {
                engram_host_core::PidLiveness::Dead
            };
            // The holder scan (proof of death) only changes a DEAD owner's
            // verdict — a live/self owner is `Serving` regardless — so we pay the
            // cold `/proc` scan only there.
            let holder = if matches!(liveness, engram_host_core::PidLiveness::Dead) {
                device_has_live_holder(&dev.device)
            } else {
                engram_host_core::DeviceHolder::NoHolder
            };
            let has_record = record_devices.contains(&dev.device);
            engram_host_core::StartupSlot {
                device: dev.device,
                liveness,
                holder,
                has_record,
            }
        })
        .collect();
    engram_host_core::classify_startup_slots(slots)
}

/// `true` if `pid` names a live process. `kill(pid, 0)` sends no signal
/// but performs the existence + permission check: `Ok` (or `EPERM`,
/// meaning the process exists but is owned by another user) → alive;
/// `ESRCH` → no such process (stale). We run as the same user that
/// CONNECTed the device, so `EPERM` shouldn't arise, but treat it as
/// "alive" defensively — never disconnect on an ambiguous signal.
fn pid_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 only probes; no memory is touched.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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

    // Liveness / self-pid guard (the function's own doc above promises
    // "a process that's no longer alive"). The pid in /sys/block/nbdN/pid
    // is the kernel-side NBD task's pid — for a netlink CONNECT issued by
    // THIS host-agent it is our own pid. A "stale" binding is one whose
    // owning process is gone; if the pid is still alive we must NOT
    // disconnect:
    //   - pid == our own pid → a live device this very generation just
    //     CONNECTed (a fresh session that won the claim race after the
    //     free-paths snapshot; pre-this-fix the sweep killed its rootfs).
    //   - pid alive but not ours → some other live owner; not stale.
    // Only a dead pid (kill(pid,0) == ESRCH) is a genuine stale binding.
    // The verdict itself is the pure `sweep_verdict` (ADR 0098 P7,
    // `engram_host_core::reattach`) — the simulator drives the same core in
    // the 731df805 scenario (a live parked survivor must never be swept). A
    // non-empty-but-unparseable pid is treated as `Dead` (stale), preserving
    // the pre-extraction fall-through.
    let self_pid = std::process::id() as i32;
    let liveness = match bound_pid.parse::<i32>() {
        Ok(pid) if pid == self_pid => engram_host_core::PidLiveness::SelfPid,
        Ok(pid) if pid_is_alive(pid) => engram_host_core::PidLiveness::Alive,
        Ok(_) | Err(_) => engram_host_core::PidLiveness::Dead,
    };
    // R6 proof-of-death (#769 gap A): a dead-owner device is only a genuine
    // stale binding if NO live process still holds its node open. Probe the
    // holder ONLY when the owner is dead (the sole arm whose verdict depends on
    // it — for a live/self/no owner the device is never swept, so the cold
    // `/proc` scan is skipped). A live (or unprovable) holder PARKs; only a
    // completed no-holder scan clears the device for DISCONNECT.
    let holder = if matches!(liveness, engram_host_core::PidLiveness::Dead) {
        device_has_live_holder(path)
    } else {
        engram_host_core::DeviceHolder::NoHolder // unused by a non-Dead verdict
    };
    match engram_host_core::sweep_verdict(liveness, holder) {
        engram_host_core::SweepAction::NotStuck => {
            tracing::info!(
                device = %path.display(),
                bound_pid = %bound_pid,
                ?liveness,
                "NBD recovery: skipping — the device's owner is live (or this very \
                 generation); not a stale binding (would be wrong to disconnect)",
            );
            return Ok(NbdRecoveryOutcome::NotStuck);
        }
        engram_host_core::SweepAction::Park => {
            // Proof of death was NOT met: a live process still holds the device
            // node open (or the scan was inconclusive). NEVER disconnect — this
            // is the 2026-07-18/19 gap-A firing: a survivor whose rehydrate was
            // missed upstream still has a live guest reading its rootfs. Leave
            // the device RECONNECTABLE (kernel binding intact) so a later
            // rehydrate/re-serve pass (the coord list or the local
            // ChainHeadRecord pass — the survivor is live ∧ unserved) adopts it
            // with zero loss. The sweep has no sandbox identity for a free-pool
            // device, hence "sandbox=?"; the alertable line names the device
            // and the holder-owner pid.
            engram_core::soft_invariant!(
                "sweep-blocked-live-holder",
                false,
                "NBD stale-binding sweep blocked: device {} is kernel-bound to a \
                 dead owner (pid {bound_pid}) but a live process still holds its \
                 node open (holder={holder:?}); left RECONNECTABLE for a later \
                 re-serve pass instead of DISCONNECTed (sandbox=? — free-pool sweep). \
                 A survivor's rehydrate was missed upstream (#769 gap A)",
                path.display(),
            );
            ::metrics::counter!(crate::metrics::SWEEP_BLOCKED_LIVE_HOLDER_TOTAL).increment(1);
            return Ok(NbdRecoveryOutcome::Parked);
        }
        engram_host_core::SweepAction::Disconnect => {}
    }

    tracing::warn!(
        device = %path.display(),
        bound_pid = %bound_pid,
        "NBD recovery: unclaimed device kernel-bound to a stale config (owning pid \
         is gone); attempting recovery via netlink NBD_CMD_DISCONNECT",
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
    // ADR 0077 phase 2: `true` on the FRESH-CREATE path (the backend is
    // born on the SHARED base `manifest_id`). Forks the manifest
    // identity to a private per-session id AT ATTACH — not lazily on
    // first flush — so concurrent same-base sessions never share a
    // version chain even before their first write. `false` for
    // resume / recovery, which attach the session's own forked id.
    fork_at_attach: bool,
) -> Result<NbdSandboxState, NbdRuntimeError> {
    // The attach-time manifest id becomes the kernel's recorded
    // `/sys/block/nbdN/backend` — INFORMATIONAL ONLY. It must never be
    // treated as the device's identity: the manifest identity legitimately
    // changes over the device's life (the ADR 0077 fork publishes flushes
    // under a private id this connect-time value never sees), which is why
    // [`reattach_manifest`] echoes the kernel's own recorded value instead
    // of re-deriving one (2026-07-13 dfa0face incident: re-deriving from
    // the live ref EINVAL'd every forked-chain survivor's rehydrate).
    let backend_id = disk_manifest_ref.manifest_id.to_string();
    let backend =
        ChunkedDiskBackend::from_blob(disk_manifest_ref, cache, store, threshold_bytes).await?;
    if fork_at_attach {
        backend.fork_manifest_identity().await;
    }
    attach_backend(backend, slot_pool, &backend_id).await
}

pub(crate) async fn attach_manifest_with_dirty_file(
    disk_manifest_ref: engram_core::types::manifest::ManifestRef,
    cache: engram_chunk_store::cache::ChunkCache,
    store: Arc<engram_chunk_store::ChunkStore>,
    slot_pool: &Arc<NbdSlotAllocator>,
    threshold_bytes: u64,
    fork_at_attach: bool,
    dirty_path: PathBuf,
) -> Result<NbdSandboxState, NbdRuntimeError> {
    let backend_id = disk_manifest_ref.manifest_id.to_string();
    let backend = ChunkedDiskBackend::from_blob_with_dirty_file(
        disk_manifest_ref,
        cache,
        store,
        threshold_bytes,
        dirty_path,
        DirtyFileOpenMode::Truncate,
    )
    .await?;
    if fork_at_attach {
        backend.fork_manifest_identity().await;
    }
    attach_backend(backend, slot_pool, &backend_id).await
}

pub(crate) async fn attach_manifest_content_with_dirty_file(
    disk_manifest_ref: engram_core::types::manifest::ManifestRef,
    manifest: &engram_chunk_store::Manifest,
    cache: engram_chunk_store::cache::ChunkCache,
    store: Arc<engram_chunk_store::ChunkStore>,
    slot_pool: &Arc<NbdSlotAllocator>,
    threshold_bytes: u64,
    dirty_path: PathBuf,
) -> Result<NbdSandboxState, NbdRuntimeError> {
    let backend_id = disk_manifest_ref.manifest_id.to_string();
    let backend = ChunkedDiskBackend::from_manifest_with_dirty_file(
        disk_manifest_ref,
        manifest,
        cache,
        store,
        threshold_bytes,
        dirty_path,
        DirtyFileOpenMode::Truncate,
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
/// On failure, returns the `NbdSlot` BACK to the caller (alongside the
/// error) rather than dropping it. Dropping it would `release()` the
/// survivor's device into the general pool — but the surviving FC may
/// still hold an open fd to that exact `/dev/nbdN`, so a released device
/// can be (a) DISCONNECTed by the startup stale-binding sweep or (b)
/// handed to an unrelated session, in both cases turning a "recoverable
/// later" survivor disk into immediate guest EIO. The caller PARKS the
/// returned slot (quarantine) so it stays out of circulation until the
/// evict_local → resume ladder recovers the session.
pub async fn reattach_manifest(
    disk_manifest_ref: engram_core::types::manifest::ManifestRef,
    cache: engram_chunk_store::cache::ChunkCache,
    store: Arc<engram_chunk_store::ChunkStore>,
    slot: NbdSlot,
    threshold_bytes: u64,
    seed_dirty: Option<Vec<(usize, Vec<u8>)>>,
) -> Result<NbdSandboxState, (NbdSlot, NbdRuntimeError)> {
    reattach_manifest_inner(
        disk_manifest_ref,
        cache,
        store,
        slot,
        threshold_bytes,
        None,
        seed_dirty,
    )
    .await
}

/// This structure groups the stable dirty file inputs for reattachment.
pub(crate) struct DirtyTierSpec {
    pub(crate) path: PathBuf,
    pub(crate) mode: DirtyFileOpenMode,
    pub(crate) seed: Option<Vec<(usize, Vec<u8>)>>,
}

pub(crate) async fn reattach_manifest_with_dirty_file(
    disk_manifest_ref: engram_core::types::manifest::ManifestRef,
    cache: engram_chunk_store::cache::ChunkCache,
    store: Arc<engram_chunk_store::ChunkStore>,
    slot: NbdSlot,
    threshold_bytes: u64,
    dirty_tier: DirtyTierSpec,
) -> Result<NbdSandboxState, (NbdSlot, NbdRuntimeError)> {
    reattach_manifest_inner(
        disk_manifest_ref,
        cache,
        store,
        slot,
        threshold_bytes,
        Some((dirty_tier.path, dirty_tier.mode)),
        dirty_tier.seed,
    )
    .await
}

async fn reattach_manifest_inner(
    disk_manifest_ref: engram_core::types::manifest::ManifestRef,
    cache: engram_chunk_store::cache::ChunkCache,
    store: Arc<engram_chunk_store::ChunkStore>,
    slot: NbdSlot,
    threshold_bytes: u64,
    dirty_file: Option<(PathBuf, DirtyFileOpenMode)>,
    // Shutdown-spool adoption (2026-07-16 RCA): the predecessor
    // generation's acked-but-un-uploaded chunks, to seed the fresh
    // backend's dirty tier. MUST be seeded before the RECONFIGURE
    // below — the kernel releases the guest's parked I/O the moment
    // it adopts our socket, and a read served before the seed would
    // observe the rolled-back base instead of the acked bytes.
    seed_dirty: Option<Vec<(usize, Vec<u8>)>>,
) -> Result<NbdSandboxState, (NbdSlot, NbdRuntimeError)> {
    // The kernel strcmp-verifies `NBD_ATTR_BACKEND_IDENTIFIER` against the
    // CONNECT-time value at RECONFIGURE (and REQUIRES one when the device
    // has it recorded) — but the connect-time value is a manifest id and
    // manifest identity legitimately changes over the device's life (the
    // ADR 0077 fork publishes flushes under a private id, so the rehydrate
    // ref's id and the connect-time id diverge for every fresh-created
    // session; 2026-07-13 dfa0face: every such rehydrate died EINVAL and
    // the survivor's disk stayed dead). Echo the kernel's own durable
    // record of the connect-time value (`/sys/block/nbdN/backend`) instead
    // of re-deriving one — deterministic for every host-agent generation.
    // The device↔sandbox pinning that actually protects against serving
    // the wrong disk is the caller's (`rootfs_device(sandbox_id)` →
    // `pool.claim` on that exact device); the kernel identifier is
    // informational. Fall back to the ref's manifest id only when sysfs
    // has no record (never netlink-configured — the RECONFIGURE will fail
    // regardless, with the right error).
    // The pure Flow B plan (ADR 0098 P7, `engram_host_core::reattach`):
    // resolve the RECONFIGURE backend identifier (echo the kernel's own
    // recorded value via the seam, else fall back to the ref's manifest id)
    // and lay out the seed-then-RECONFIGURE ordering as explicit steps.
    let kernel = HostNbdKernel;
    let plan = engram_host_core::plan_reattach(
        disk_manifest_ref.manifest_id,
        kernel.backend_identifier(slot.path()),
        seed_dirty.is_some(),
    );
    if plan.used_identifier_fallback {
        tracing::warn!(
            device = %slot.path().display(),
            "no kernel-recorded NBD backend identifier; falling back to the \
             rehydrate ref's manifest id",
        );
    }
    let stable_dirty_file = dirty_file.is_some();
    let backend_result = match dirty_file {
        Some((path, mode)) => {
            ChunkedDiskBackend::from_blob_with_dirty_file(
                disk_manifest_ref,
                cache,
                store,
                threshold_bytes,
                path,
                mode,
            )
            .await
        }
        None => {
            ChunkedDiskBackend::from_blob(disk_manifest_ref, cache, store, threshold_bytes).await
        }
    };
    let backend = match backend_result {
        Ok(backend) => {
            if stable_dirty_file {
                backend.retain_dirty_file().await;
            }
            Arc::new(backend)
        }
        Err(e) => return Err((slot, e.into())),
    };
    // Verify-on-read probe target (ADR 0098 P7 rider): the first seeded chunk,
    // captured BEFORE `adopt_unflushed` consumes the seed vec. `None` unless a
    // spool was adopted (and non-empty), so a clean rehydrate pays nothing.
    let probe: Option<(usize, Vec<u8>)> =
        engram_host_core::first_seeded_probe(seed_dirty.as_deref())
            .map(|(idx, bytes)| (idx, bytes.to_vec()));
    // Execute the PLAN's steps in the plan's own order (issue #810 finding
    // 1a). The previous shape hand-ordered these calls and carried a
    // `debug_assert!(plan.seed_precedes_reconfigure())` — a release no-op
    // that checked the PLAN's internal order, not the driver's execution
    // order, so a future driver reorder would have silently reintroduced
    // the 2026-07-16 rolled-back-base read window. Iterating `plan.steps`
    // makes the ordering structurally unbypassable (the ReapList lesson):
    // the driver cannot reorder what it does not sequence.
    // (The error is captured and returned ONCE below the loop — `slot` moves
    // into the park-shaped `Err((slot, e))`, which the borrow checker rightly
    // refuses inside a loop that reads `slot.path()` on later iterations.)
    let mut seed_dirty = seed_dirty;
    let mut handle = None;
    let mut step_err: Option<NbdRuntimeError> = None;
    'steps: for step in &plan.steps {
        match step {
            engram_host_core::ReattachStep::SeedDirtyTier => {
                let chunks = seed_dirty.take().expect(
                    "plan_reattach emits SeedDirtyTier iff a seed exists (host-core pinned)",
                );
                let count = chunks.len();
                // #810: adoption is ATOMIC and fallible — one out-of-shape
                // chunk refuses the whole spool (adopting a subset silently
                // rolls back the missing chunk's acked write). Refusal parks
                // the survivor with the spool preserved on disk.
                let bytes = match backend.adopt_unflushed(chunks).await {
                    Ok(b) => b,
                    Err(e) => {
                        step_err = Some(NbdRuntimeError::Io(io::Error::other(format!(
                            "rehydrate: spool adoption refused; parking the survivor \
                             rather than serving with rolled-back acked writes: {e}"
                        ))));
                        break 'steps;
                    }
                };
                tracing::info!(
                    device = %slot.path().display(),
                    chunks = count,
                    bytes,
                    "rehydrate: adopted predecessor's shutdown-spool dirty chunks \
                     ahead of RECONFIGURE",
                );
            }
            engram_host_core::ReattachStep::VerifySeed => {
                // Verify-on-read (ADR 0098 P7 rider): prove the seeded acked
                // bytes are readable at their offset through the backend the
                // RECONFIGURE below is about to hand the kernel — a
                // single-chunk probe (a dirty-tier read), NOT a
                // full-disk scan (latency is non-negotiable). This step runs
                // strictly BEFORE Reconfigure: the kernel releases the
                // guest's parked I/O the instant it adopts our socket, so a
                // post-RECONFIGURE probe races the live guest's own writes to
                // the probed (hottest) chunk — the 2026-07-21 incident parked
                // and destroyed a healthy, actively-writing survivor on every
                // host roll this way, rewinding the very acked writes the
                // spool preserved. A mismatch here means the adoption did not
                // land in the backend about to be served; park the survivor
                // with the KERNEL CONFIG UNTOUCHED (still dead-parked), so a
                // later rehydrate attempt can still recover the device. The
                // device-plane O_DIRECT check is the FC regression lane's job.
                let Some((idx, expected)) = probe.as_ref() else {
                    // An adopted-but-empty spool seeds nothing to verify.
                    continue;
                };
                let idx = *idx;
                let chunk_size = backend.chunk_size();
                let offset = idx as u64 * chunk_size;
                // Clamp the read to the device extent: the final chunk (or a
                // device smaller than one chunk — e.g. the 4 MiB test images)
                // is shorter than chunk_size, and reading a full chunk_size
                // there overruns total_bytes and errors. `probe_matches` is a
                // prefix compare, so a full (possibly-partial) chunk read is
                // enough to prove the seeded bytes would be served.
                let read_len = chunk_size.min(backend.total_bytes().saturating_sub(offset));
                match backend.read(offset, read_len).await {
                    Ok(bytes) if engram_host_core::probe_matches(&bytes, expected) => {}
                    Ok(_) => {
                        step_err = Some(NbdRuntimeError::Io(io::Error::other(format!(
                            "verify-on-read: {} backend read of seeded chunk {idx} did \
                             not return the adopted acked bytes (seed failed to land); \
                             parking the survivor with the kernel config untouched",
                            slot.path().display(),
                        ))));
                        break 'steps;
                    }
                    Err(e) => {
                        step_err = Some(NbdRuntimeError::Io(io::Error::other(format!(
                            "verify-on-read: {} readback of seeded chunk {idx} failed \
                             before RECONFIGURE: {e}",
                            slot.path().display(),
                        ))));
                        break 'steps;
                    }
                }
            }
            engram_host_core::ReattachStep::Reconfigure => {
                match reattach(backend.clone(), slot.path(), &plan.backend_id).await {
                    Ok(h) => handle = Some(h),
                    Err(e) => {
                        step_err = Some(e);
                        break 'steps;
                    }
                }
                // #810 finding 1b (hygiene half): fresh CONNECT invalidates
                // the reused minor's page cache (the 85e0298a cross-tenant
                // class, documented at `attach_backend`) — but the reattach
                // path never did, leaving the PREDECESSOR GENERATION's
                // cached pages as exactly the stale view a surviving guest
                // must never read. Same hard-error posture as CONNECT:
                // serving without the invalidation risks silent corruption,
                // strictly worse than a parked survivor.
                let dev = slot.path().to_path_buf();
                if let Err(e) = tokio::task::spawn_blocking(move || flush_block_device_cache(&dev))
                    .await
                    .map_err(|e| io::Error::other(format!("BLKFLSBUF task join: {e}")))
                    .and_then(|r| r)
                {
                    step_err = Some(NbdRuntimeError::Io(io::Error::other(format!(
                        "invalidate page cache (BLKFLSBUF) on reattached {}: {e}",
                        slot.path().display()
                    ))));
                    break 'steps;
                }
            }
        }
    }
    if let Some(e) = step_err {
        // A step can fail AFTER the RECONFIGURE handed the kernel our socket
        // (today: the BLKFLSBUF invalidation). Dropping the handle would
        // netlink-DISCONNECT the device — immediate EIO for the surviving
        // guest the park exists to protect (2026-07-21: exactly this drop
        // disconnected a parked survivor's device out from under its FC and
        // then confused the startup classification barrier). Abandon the
        // serve loop instead: the kernel observes a dead connection and
        // re-parks guest I/O under `dead_conn_timeout`, keeping the survivor
        // recoverable (a later rehydrate attempt or the evict_local → resume
        // ladder). Nothing serves reads meanwhile, so the failed
        // invalidation cannot leak stale pages to the guest.
        if let Some(h) = handle {
            tracing::warn!(
                device = %slot.path().display(),
                "reattach step failed after RECONFIGURE; abandoning the serve \
                 loop in place (no netlink disconnect) so the kernel re-parks \
                 guest I/O instead of EIO-ing the survivor",
            );
            h.abandon();
        }
        return Err((slot, e));
    }
    let handle = handle.expect("plan_reattach always emits Reconfigure (host-core pinned)");
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
    // 2026-07-16 session-85e0298a corruption RCA: `/dev/nbdN` minors are
    // REUSED across tenants (ADR 0049 pool) and FC reads the device through
    // the host page cache, which the kernel does NOT reliably invalidate
    // across disconnect/reconnect. A fresh tenant on a reused slot could be
    // served the PREVIOUS tenant's cached pages — including pages whose
    // writeback had failed during that tenant's teardown (observed as a
    // corrupt-inode-bitmap CRC failure 17 minutes into a fresh session).
    // The post-copy migration restore already guards this exact class with
    // BLKFLSBUF; do the same on every fresh CONNECT, before the caller
    // hands the device to FC. Hard error: serving without the invalidation
    // risks silent cross-tenant corruption, which is strictly worse than a
    // failed create.
    let dev = slot.path().to_path_buf();
    if let Err(e) = tokio::task::spawn_blocking(move || flush_block_device_cache(&dev))
        .await
        .map_err(|e| io::Error::other(format!("BLKFLSBUF task join: {e}")))
        .and_then(|r| r)
    {
        return Err(NbdRuntimeError::Io(io::Error::other(format!(
            "invalidate page cache (BLKFLSBUF) on freshly connected {}: {e}",
            slot.path().display()
        ))));
    }
    Ok(NbdSandboxState {
        scheduler: None,
        backend,
        handle,
        slot,
    })
}

/// `BLKFLSBUF`: write back and invalidate the kernel page cache for a
/// block device. Used on every fresh NBD CONNECT (cross-tenant slot
/// hygiene, above) and by the post-copy migration restore before it
/// lands `state.bin` (its stale-probe comment documents the corruption
/// class).
pub fn flush_block_device_cache(device: &Path) -> io::Result<()> {
    // libc::Ioctl is the per-target request type: c_ulong on gnu,
    // c_int on musl (the prod artifact) — a bare c_ulong breaks
    // the musl cross-compile.
    const BLKFLSBUF: libc::Ioctl = 0x1261; // _IO(0x12, 97)
    let f = std::fs::OpenOptions::new().read(true).open(device)?;
    // SAFETY: BLKFLSBUF takes no argument; the fd is valid for the
    // duration of the call.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), BLKFLSBUF) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
    serve_at(
        backend,
        nbd_device,
        backend_id,
        ConnectMode::Connect,
        &HostNbdKernel,
    )
    .await
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
    let started = crate::time_source::metrics_now();
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let handle = serve_at(
            backend.clone(),
            nbd_device,
            backend_id,
            ConnectMode::Reconfigure,
            &HostNbdKernel,
        )
        .await?;
        // An adopted socket stays open (the kernel holds its dup); a
        // rejected one (kernel-swallowed ENOSPC) EOFs the serve loop. The
        // EOF is near-instant in the KERNEL, but observing the serve task
        // FINISH is subject to runtime scheduling jitter — a single short
        // check raced it under load: a rejection whose EOF surfaced after
        // the window read as a FALSE "adopted", `reattach` returned Ok, and
        // the guest's parked I/O never resumed (flaky
        // survivor_reconfigure_resumes_parked_io; in prod, a hung guest).
        // Poll for the rejection EOF over a generous window — retry the
        // moment it finishes, declare adoption only if it stays alive.
        const ADOPT_CONFIRM: std::time::Duration = std::time::Duration::from_secs(2);
        let confirm_deadline = crate::time_source::metrics_now() + ADOPT_CONFIRM;
        let mut rejected = false;
        while crate::time_source::metrics_now() < confirm_deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if handle
                .serve_task
                .as_ref()
                .map(|t| t.is_finished())
                .unwrap_or(true)
            {
                rejected = true;
                break;
            }
        }
        if !rejected {
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
    // ADR 0098 P7 (Flow B): the kernel control plane behind the seam. Prod
    // passes `&HostNbdKernel`; the CONNECT/RECONFIGURE genl round-trips run on
    // `spawn_blocking` inside the impl. serve_at owns the socketpair lifetime
    // (below), so it stays the choke point for the serve loop.
    kernel: &dyn NbdKernel,
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

    // 2. Configure (or re-arm) the device through the kernel seam. The
    //    kernel dups the socket fd, runs its own receive machinery (no
    //    NBD_DO_IT thread), and parks guest I/O for `dead_conn_timeout`
    //    whenever the connection dies — the pod-roll survival contract. The
    //    serve fd is the kernel-side half, alive across the await because
    //    serve_at owns it until `drop(kernel_side)` below.
    let serve_fd = kernel_side.as_raw_fd();
    match mode {
        ConnectMode::Connect => {
            kernel
                .connect(NbdConnectRequest {
                    device: nbd_device,
                    serve_fd,
                    size_bytes: total_bytes,
                    block_size: NBD_BLOCK_SIZE,
                    timeout_secs: nbd_kernel_timeout_secs(),
                    dead_conn_timeout_secs: nbd_dead_conn_timeout_secs(),
                    backend_identifier: backend_id,
                })
                .await?;
        }
        ConnectMode::Reconfigure => {
            kernel
                .reconfigure(NbdReconfigureRequest {
                    device: nbd_device,
                    serve_fd,
                    timeout_secs: nbd_kernel_timeout_secs(),
                    dead_conn_timeout_secs: nbd_dead_conn_timeout_secs(),
                    backend_identifier: backend_id,
                })
                .await?;
        }
    }
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

/// Serve NBD requests on `stream` until the kernel disconnects or the
/// stream errors. The kernel issues many in-flight requests on one
/// socket (correlated by `handle`), so we PIPELINE (ADR 0071): a reader
/// parses headers and dispatches each request to a bounded pool of
/// handler tasks; a single writer task serializes the replies back onto
/// the wire. Out-of-order completion is legal on the wire (the handle
/// correlates), but concurrent writes to one socket are not — hence the
/// single writer.
///
/// ADR 0018 commit 12m: each in-flight request holds an `InFlightGuard`
/// (`++count`); the guard rides with its reply to the writer and drops
/// only AFTER the reply is flushed, so the snapshot pipeline's
/// `backend.wait_idle().await` (called after `inner.pause()` to drain
/// the virtio→kernel-NBD→userspace pipeline) counts a request as
/// in-flight until its bytes are on the wire. `ENGRAM_NBD_SERVE_CONCURRENCY=1`
/// reproduces the legacy strictly-serial loop.
async fn serve_loop(backend: Arc<ChunkedDiskBackend>, stream: TokioUnixStream) {
    let in_flight = backend.in_flight_tracker();
    let concurrency = nbd_serve_concurrency();
    let sem = Arc::new(Semaphore::new(concurrency));
    let (mut read_half, write_half) = stream.into_split();
    // Bounded so a kernel that stops draining replies backpressures the
    // handlers (which keep their guards+permits) rather than buffering
    // unboundedly. Capacity = concurrency: every in-flight reply fits, so
    // a keeping-up kernel never blocks a handler on send.
    let (reply_tx, reply_rx) = mpsc::channel::<ReplyMsg>(concurrency.max(1));
    let writer = tokio::spawn(writer_loop(write_half, reply_rx));

    loop {
        let mut header = [0u8; REQUEST_HEADER_LEN];
        match read_half.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                tracing::info!("NBD serve loop: kernel closed socket cleanly");
                break;
            }
            Err(e) => {
                tracing::warn!(error = %e, "NBD serve loop: read header failed");
                break;
            }
        }
        let req = match NbdRequest::parse(&header) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "NBD serve loop: malformed request header");
                break;
            }
        };

        // Disconnect is a sentinel to end the loop, not a tracked op.
        if matches!(req.command, NbdCommand::Disconnect) {
            tracing::info!("NBD client requested disconnect");
            break;
        }

        // A WRITE's payload trails its header on the read half, so it must
        // be consumed here IN ORDER — it cannot be deferred to a
        // concurrent handler without desyncing the stream.
        let write_data = if matches!(req.command, NbdCommand::Write) {
            if req.length > MAX_REQUEST_PAYLOAD_BYTES || (req.length as u64) > backend.total_bytes()
            {
                tracing::error!(
                    length = req.length,
                    "NBD serve loop: WRITE payload length exceeds request or disk bound"
                );
                break;
            }
            let mut data = vec![0u8; req.length as usize];
            if let Err(e) = read_half.read_exact(&mut data).await {
                tracing::warn!(error = %e, "NBD write payload read failed");
                break;
            }
            Some(data)
        } else {
            None
        };

        // Record in-flight + take a concurrency permit BEFORE spawning.
        // Order matters: the guard is held across `acquire_owned`, so a
        // request the reader has already accepted (header — and, for a
        // WRITE, payload — consumed) counts toward `wait_idle` even while
        // it waits for a slot. `acquire_owned` also backpressures the
        // reader once `concurrency` handlers are outstanding, bounding
        // both task count and buffered reply memory.
        let guard = in_flight.enter();
        let permit = match Arc::clone(&sem).acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // semaphore closed (shutdown)
        };
        let backend = backend.clone();
        let reply_tx = reply_tx.clone();
        tokio::spawn(async move {
            let msg = handle_request(&backend, req, write_data, guard, permit).await;
            // If the writer is gone (connection torn down), the send
            // fails and `msg` — including its guard — drops here,
            // releasing the in-flight count.
            let _ = reply_tx.send(msg).await;
        });
    }

    // Stop accepting and drop our sender. The writer exits once every
    // outstanding handler's sender clone drops — i.e. once all in-flight
    // replies have drained — so awaiting it gives a clean teardown.
    drop(reply_tx);
    let _ = writer.await;
}

/// A computed NBD reply awaiting serialization onto the wire. The
/// per-request `InFlightGuard` and concurrency `_permit` ride along and
/// drop only after the writer flushes the reply (ADR 0071 / 0018).
struct ReplyMsg {
    header: [u8; REPLY_HEADER_LEN],
    /// `Some` for READ responses (the data follows the header); `None`
    /// for WRITE / FLUSH / TRIM, whose reply is the bare header.
    payload: Option<Bytes>,
    _guard: InFlightGuard,
    _permit: OwnedSemaphorePermit,
}

/// Compute one request's reply off the read path. Runs concurrently with
/// other handlers (bounded by the serve-loop semaphore); the backend
/// serializes dirty-buffer mutations under its own lock, so concurrent
/// read/write handlers cannot observe torn state.
async fn handle_request(
    backend: &ChunkedDiskBackend,
    req: NbdRequest,
    write_data: Option<Vec<u8>>,
    guard: InFlightGuard,
    permit: OwnedSemaphorePermit,
) -> ReplyMsg {
    let (reply, payload) = match req.command {
        NbdCommand::Read => {
            // `backend.read` self-bounds via the chunk-fetch retry budget
            // (per-attempt timeout × max attempts), so a stalled/missing
            // chunk surfaces as `Err` → EIO in bounded time. The kernel
            // NBD_SET_TIMEOUT (see `spawn`) backstops a wedged daemon that
            // never replies at all.
            //
            // Same noise-floor argument as the write ack below, at a
            // hotter rate: reads burst to ~76k/s per session, so the
            // labels stay `&'static str` (no per-op alloc) and the
            // observation happens ONCE here per NBD request — never
            // inside the per-chunk loop `backend.read` runs for
            // fragmented requests.
            let started = crate::time_source::metrics_now();
            let outcome = backend.read(req.offset, req.length as u64).await;
            ::metrics::histogram!(
                crate::metrics::NBD_READ_SECONDS,
                "outcome" => if outcome.is_ok() { "ok" } else { "eio" },
            )
            .record(started.elapsed().as_secs_f64());
            ::metrics::histogram!(crate::metrics::NBD_READ_BYTES).record(req.length as f64);
            match outcome {
                Ok(bytes) => (NbdReply::ok(req.handle), Some(bytes)),
                Err(e) => {
                    tracing::warn!(error = %e, "NBD read failed");
                    (NbdReply::eio(req.handle), None)
                }
            }
        }
        NbdCommand::Write => {
            let data = write_data.unwrap_or_default();
            // ADR 0110 rollout gate (tradeoff 1). `backend.write` now
            // completes a `pwrite` into the dirty file BEFORE it acks,
            // where the RAM map only touched memory. Under memory
            // pressure the kernel can throttle that `pwrite` into
            // writeback, which the RAM map never did — so this is the one
            // latency the honesty newly puts on the guest's critical path.
            //
            // Two clock reads and one bucket increment, against a write
            // that may materialize a whole 16 MiB chunk: the measurement
            // is far below the noise floor of the thing it measures.
            //
            // `metrics_now` is the sanctioned monotonic carve-out for
            // data-plane histograms (ADR 0098 D1); a raw `Instant::now`
            // is a hard clippy error in this crate.
            let started = crate::time_source::metrics_now();
            let outcome = backend.write(req.offset, &data).await;
            ::metrics::histogram!(
                crate::metrics::NBD_WRITE_ACK_SECONDS,
                "outcome" => if outcome.is_ok() { "ok" } else { "eio" },
            )
            .record(started.elapsed().as_secs_f64());
            ::metrics::histogram!(crate::metrics::NBD_WRITE_BYTES).record(data.len() as f64);
            match outcome {
                Ok(()) => (NbdReply::ok(req.handle), None),
                Err(e) => {
                    tracing::warn!(error = %e, "NBD write failed");
                    (NbdReply::eio(req.handle), None)
                }
            }
        }
        // Acked writes already survive process death in the dirty file.
        // FLUSH has no more work to do. Periodic uploads cover node death.
        // TRIM remains a no-op so the kernel can keep offering it.
        NbdCommand::Flush | NbdCommand::Trim => (NbdReply::ok(req.handle), None),
        // Handled in the reader before dispatch; unreachable here.
        NbdCommand::Disconnect => (NbdReply::ok(req.handle), None),
    };
    ReplyMsg {
        header: reply.encode(),
        payload,
        _guard: guard,
        _permit: permit,
    }
}

/// Drain computed replies onto the write half, one at a time, in
/// completion order. Owning the only handle to the write half is what
/// keeps the wire well-formed under concurrent handlers. Each message's
/// guard+permit drop at the end of the iteration — AFTER the flush — so
/// `wait_idle()` observes the request as in-flight until then.
async fn writer_loop(mut write_half: OwnedWriteHalf, mut reply_rx: mpsc::Receiver<ReplyMsg>) {
    while let Some(msg) = reply_rx.recv().await {
        if let Err(e) = write_half.write_all(&msg.header).await {
            tracing::warn!(error = %e, "NBD reply header write failed");
            return;
        }
        if let Some(payload) = &msg.payload {
            if let Err(e) = write_half.write_all(payload).await {
                tracing::warn!(error = %e, "NBD reply payload write failed");
                return;
            }
        }
        // `msg` (guard + permit) drops here, after the reply is on the wire.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk_daemon::nbd::{NBD_REPLY_MAGIC, NBD_REQUEST_MAGIC};
    use engram_chunk_store::cache::ChunkCacheConfig;
    use engram_chunk_store::manifest::{
        ChunkRef, ChunkSize, Manifest, ManifestKind, MANIFEST_SCHEMA_VERSION,
    };
    use engram_chunk_store::{ChunkCache, ChunkStore};
    use engram_core::traits::BlobStorage;
    use engram_core::types::manifest::ManifestRef;
    use engram_storage_local::LocalBlobStorage;
    use tokio::net::unix::OwnedReadHalf;

    /// The stale-binding sweep's liveness guard: a binding owned by a
    /// LIVE process (in particular this very process — a session that
    /// won the slot after the free-paths snapshot) must read as alive so
    /// `recover_one_stuck_device` skips it. Only a genuinely-dead pid
    /// (the actual "stale" case) reads as not-alive and is disconnected.
    #[test]
    fn pid_is_alive_distinguishes_live_from_dead() {
        // Our own pid is alive (this is the self-pid skip case).
        assert!(pid_is_alive(std::process::id() as i32));
        // pid 1 (init) always exists on Linux; kill(1,0) → 0 or EPERM,
        // both of which we treat as alive.
        assert!(pid_is_alive(1));
        // A pid far above the kernel's pid_max is guaranteed unused →
        // ESRCH → dead. (pid_max is at most ~4M on stock Linux.)
        assert!(!pid_is_alive(i32::MAX));
        // Non-positive pids are never a real process.
        assert!(!pid_is_alive(0));
        assert!(!pid_is_alive(-1));
    }

    /// Build a 28-byte NBD request header (no payload), flags = 0.
    fn req_bytes(cmd: u16, handle: u64, offset: u64, length: u32) -> [u8; REQUEST_HEADER_LEN] {
        let mut b = [0u8; REQUEST_HEADER_LEN];
        b[0..4].copy_from_slice(&NBD_REQUEST_MAGIC.to_be_bytes());
        b[6..8].copy_from_slice(&cmd.to_be_bytes());
        b[8..16].copy_from_slice(&handle.to_be_bytes());
        b[16..24].copy_from_slice(&offset.to_be_bytes());
        b[24..28].copy_from_slice(&length.to_be_bytes());
        b
    }

    /// Read one reply: a 16-byte header, plus `payload_len` payload bytes on a
    /// successful READ. Returns `(handle, error, payload)`.
    async fn read_reply(rd: &mut OwnedReadHalf, payload_len: usize) -> (u64, u32, Vec<u8>) {
        let mut hdr = [0u8; REPLY_HEADER_LEN];
        rd.read_exact(&mut hdr).await.unwrap();
        assert_eq!(
            u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]),
            NBD_REPLY_MAGIC,
        );
        let error = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        let handle = u64::from_be_bytes([
            hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15],
        ]);
        let mut payload = vec![0u8; if error == 0 { payload_len } else { 0 }];
        if error == 0 && payload_len > 0 {
            rd.read_exact(&mut payload).await.unwrap();
        }
        (handle, error, payload)
    }

    /// A 3-chunk disk backend (chunks of 0xaa / 0xbb / 0xcc) over a local blob
    /// store. The returned `TempDir` keeps the store + cache dirs alive.
    async fn three_chunk_backend() -> (ChunkedDiskBackend, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let cs = 4096u64;
        let h0 = store.put_chunk(&vec![0xaa_u8; cs as usize]).await.unwrap();
        let h1 = store.put_chunk(&vec![0xbb_u8; cs as usize]).await.unwrap();
        let h2 = store.put_chunk(&vec![0xcc_u8; cs as usize]).await.unwrap();
        let manifest = Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Disk,
            chunk_size: ChunkSize::bytes(cs),
            total_bytes: 3 * cs,
            chunks: vec![
                ChunkRef {
                    offset: 0,
                    hash: h0,
                },
                ChunkRef {
                    offset: cs,
                    hash: h1,
                },
                ChunkRef {
                    offset: 2 * cs,
                    hash: h2,
                },
            ],
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap();
        (backend, dir)
    }

    /// ADR 0071 (#1): the pipelined serve loop services several in-flight
    /// reads concurrently, returns well-formed replies correlated by handle,
    /// and — once every reply is drained — releases all in-flight guards so
    /// the snapshot drain (`wait_idle`) settles. A leaked guard would hang it.
    #[tokio::test]
    async fn serve_loop_pipelines_reads_correlated_and_drains() {
        let (backend, _dir) = three_chunk_backend().await;
        let backend = Arc::new(backend);
        let in_flight = backend.in_flight_tracker();
        let (client, server) = TokioUnixStream::pair().unwrap();
        let serve = tokio::spawn(serve_loop(backend, server));
        let (mut rd, mut wr) = client.into_split();

        let cs = 4096u32;
        // Issue all three reads before reading any reply — exercise the
        // pipeline (handlers run concurrently; replies may complete in any
        // order, correlated by handle).
        for (handle, off) in [(10u64, 0u64), (20, 4096), (30, 8192)] {
            wr.write_all(&req_bytes(0, handle, off, cs)).await.unwrap();
        }
        let mut by_handle = std::collections::HashMap::new();
        for _ in 0..3 {
            let (handle, error, payload) = read_reply(&mut rd, cs as usize).await;
            assert_eq!(error, 0, "handle {handle} errored");
            by_handle.insert(handle, payload);
        }
        assert!(by_handle[&10].iter().all(|b| *b == 0xaa));
        assert!(by_handle[&20].iter().all(|b| *b == 0xbb));
        assert!(by_handle[&30].iter().all(|b| *b == 0xcc));

        // Drain invariant: all replies are off the wire, so all guards have
        // dropped — `wait_idle` must settle promptly (a leak would hang).
        tokio::time::timeout(std::time::Duration::from_secs(5), in_flight.wait_idle())
            .await
            .expect("wait_idle did not settle after replies drained");

        // Clean disconnect ends the serve loop.
        wr.write_all(&req_bytes(2, 0, 0, 0)).await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), serve).await;
    }

    /// ADR 0071 (#1): a WRITE round-trips through the pipeline — its payload is
    /// consumed in order by the reader, applied by the backend, and a later
    /// READ of the same range reads it back.
    #[tokio::test]
    async fn serve_loop_write_then_read_round_trips() {
        let (backend, _dir) = three_chunk_backend().await;
        let backend = Arc::new(backend);
        let (client, server) = TokioUnixStream::pair().unwrap();
        let serve = tokio::spawn(serve_loop(backend, server));
        let (mut rd, mut wr) = client.into_split();

        // WRITE 4096 bytes of 0xff at offset 0 (NBD_CMD_WRITE = 1): header then
        // payload.
        wr.write_all(&req_bytes(1, 1, 0, 4096)).await.unwrap();
        wr.write_all(&vec![0xff_u8; 4096]).await.unwrap();
        let (h, err, _) = read_reply(&mut rd, 0).await;
        assert_eq!((h, err), (1, 0), "write ack");

        // READ it back from the dirty buffer.
        wr.write_all(&req_bytes(0, 2, 0, 4096)).await.unwrap();
        let (h, err, payload) = read_reply(&mut rd, 4096).await;
        assert_eq!((h, err), (2, 0));
        assert!(
            payload.iter().all(|b| *b == 0xff),
            "read-back sees the write"
        );

        wr.write_all(&req_bytes(2, 0, 0, 0)).await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), serve).await;
    }

    #[tokio::test]
    async fn serve_loop_rejects_oversized_write_before_payload_allocation() {
        let (backend, _dir) = three_chunk_backend().await;
        let backend = Arc::new(backend);
        let assertion_backend = backend.clone();
        let (client, server) = TokioUnixStream::pair().unwrap();
        let serve = tokio::spawn(serve_loop(backend, server));
        let (mut rd, mut wr) = client.into_split();

        wr.write_all(&req_bytes(1, 1, 0, u32::MAX)).await.unwrap();
        let mut reply = [0u8; REPLY_HEADER_LEN];
        let read =
            tokio::time::timeout(std::time::Duration::from_secs(2), rd.read_exact(&mut reply))
                .await
                .expect("oversized WRITE did not tear down the connection promptly");
        assert_eq!(read.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        tokio::time::timeout(std::time::Duration::from_secs(2), serve)
            .await
            .expect("serve loop did not join promptly")
            .expect("serve loop task panicked");

        let contents = assertion_backend.read(0, 4096).await.unwrap();
        assert!(contents.iter().all(|byte| *byte == 0xaa));
    }
}
