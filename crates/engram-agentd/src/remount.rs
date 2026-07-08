//! ADR 0035 §3: re-mount the RO bundle mounts at session bind.
//!
//! On a fresh session create the host may have `patch_drive`d an aux
//! bundle drive to a newer content generation while the VM was
//! load-paused. The block device now carries different bytes than the
//! squashfs superblock the guest parsed at base-snapshot capture —
//! reads through the stale mount would EIO (the 2026-06-03 incident's
//! failure mode, then caused by a host roll mutating the backing file
//! at a fixed path). A plain umount + mount re-parses the swapped
//! device, after which the mount serves the new generation.
//!
//! The asymmetry that makes "just try it" correct (ADR 0035 §3):
//!
//! - **Fresh create:** nothing holds the bundle open (capture happens
//!   before any harness bind), the umount succeeds, the remount picks
//!   up the swapped device.
//! - **Resume of an evicted session:** the host never swaps the drive
//!   (the device still matches the mounted superblock), and live
//!   processes may hold fds into the bundle — the umount EBUSYs, which
//!   we treat as "mount is in use and still correct; keep it".
//!
//! Best-effort like the rest of bundle activation (ADR 0027): a failed
//! remount must never fail the session bind — but it IS the loud-log
//! case, because a swapped device under an unmountable mount is the
//! incident class again.

/// What one bundle mountpoint's remount did, for logging.
#[derive(Debug, PartialEq, Eq)]
pub enum RemountOutcome {
    /// umount + mount succeeded; the mount now reflects the device.
    Remounted,
    /// umount said EBUSY — in-use mount (resume path); kept as-is.
    KeptBusy,
    /// Something else failed; the message says what. The bind
    /// proceeds, but `activate()`'s probes may now see a stale or
    /// broken mount — worth a loud warning upstream.
    Failed(String),
}

/// ADR 0055: the reserved dynamic-mount slot prefix the init shim mounts skill
/// squashfs at (`/opt/engram/dyn/<i>`; see `AuxRoDrive::slot_guest_mount` and
/// the stage-1 init script (engram-rootfs-materializer)). Every per-session skill swap lands under
/// this prefix, so re-mounting the prefix covers them all uniformly.
#[cfg(target_os = "linux")]
const DYN_MOUNT_PREFIX: &str = "/opt/engram/dyn/";

/// Re-mount whatever bundle mounts exist. Returns one `(mountpoint,
/// outcome)` per *mounted* bundle path — absent mounts (image without
/// bundles, VZ guests) produce no entry.
#[cfg(target_os = "linux")]
pub fn remount_bundle_mounts() -> Vec<(String, RemountOutcome)> {
    let mounts = match std::fs::read_to_string("/proc/self/mounts") {
        Ok(s) => s,
        Err(e) => {
            return vec![(
                "/proc/self/mounts".into(),
                RemountOutcome::Failed(format!("read: {e}")),
            )]
        }
    };
    let mut report = Vec::new();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(device), Some(target), Some(fs_type)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if fs_type != "squashfs" || !target.starts_with(DYN_MOUNT_PREFIX) {
            continue;
        }
        report.push((target.to_string(), remount_one(device, target)));
    }
    report
}

#[cfg(target_os = "linux")]
fn remount_one(device: &str, target: &str) -> RemountOutcome {
    use nix::mount::{mount, umount2, MntFlags, MsFlags};
    match umount2(target, MntFlags::empty()) {
        Err(nix::errno::Errno::EBUSY) => return RemountOutcome::KeptBusy,
        Err(e) => return RemountOutcome::Failed(format!("umount {target}: {e}")),
        Ok(()) => {}
    }
    match mount(
        Some(device),
        target,
        Some("squashfs"),
        MsFlags::MS_RDONLY,
        None::<&str>,
    ) {
        Ok(()) => RemountOutcome::Remounted,
        Err(e) => RemountOutcome::Failed(format!("mount {device} -> {target}: {e}")),
    }
}

/// Non-Linux stub (agentd only ever runs in Linux guests; this keeps
/// the crate compiling for host-side unit tests on macOS).
#[cfg(not(target_os = "linux"))]
pub fn remount_bundle_mounts() -> Vec<(String, RemountOutcome)> {
    Vec::new()
}

/// [`remount_bundle_mounts`] + the standard per-outcome logging. Shared
/// by the session-bind path (`HarnessSupervisor::spawn`) and the ADR
/// 0080 `RefreshAgent` path, which both need the same "re-parse every
/// possibly-swapped device" dance with the same loudness contract.
pub fn remount_and_log() {
    for (target, outcome) in remount_bundle_mounts() {
        match outcome {
            RemountOutcome::Remounted => {
                tracing::info!(%target, "ADR 0035: bundle mount re-parsed");
            }
            RemountOutcome::KeptBusy => {
                tracing::debug!(%target, "ADR 0035: bundle mount in use (resume); kept");
            }
            RemountOutcome::Failed(e) => {
                // Loud: a swapped device under a stale mount is the
                // 2026-06-03 incident class — the session will come up
                // with broken skills (or a stale agentd) if this fires
                // after a swap.
                tracing::error!(%target, error = %e, "ADR 0035: bundle remount FAILED");
            }
        }
    }
}
