//! Boot-time readahead tuning for the guest's virtio disks.
//!
//! A bigger readahead window than the kernel default (128 KiB) converts
//! long chains of small dependent reads (JVM classloading is the measured
//! worst case) into fewer, larger reads. The 4 MiB value was measured
//! against a network-backed rootfs; it is not re-measured against the
//! local thin volume.
//!
//! Applied once at agentd startup. A fresh boot captures the setting
//! into the base snapshot, so every session restored from that snapshot
//! inherits it (sysfs state rides the VM memory image). Sessions
//! restored from snapshots captured before this change keep the old
//! default until their image re-captures — agentd is baked per-image, so
//! a restored VM runs the agentd that booted it.

use std::path::Path;

/// 4 MiB — the measured knee of the readahead sweep.
const READ_AHEAD_KB: &str = "4096";

/// ADR 0112: the swap device's readahead. Swap-in is random 4 KiB
/// against a local volume — the 4 MiB window is sized for the
/// rootfs's serial classloading chains, and #1044's profiling showed
/// large readahead INVERTS under reclaim pressure, which is the only
/// regime the swap device ever serves. Kernel default, not zero:
/// block readahead barely matters here, it just must not be 4 MiB.
const SWAP_READ_AHEAD_KB: &str = "128";

/// Set `read_ahead_kb` on every virtio block device. Best-effort by
/// design: a missing sysfs tree (non-Linux backends, tests) or an
/// unwritable file logs and moves on — tuning must never block agent
/// bring-up.
///
/// ADR 0112 carve-out: the swap drive (the only writable non-vda
/// disk, see `swap::find_swap_device`) gets [`SWAP_READ_AHEAD_KB`]
/// instead of the rootfs window.
pub fn apply_block_readahead() {
    let swap_dev = crate::swap::find_swap_device(Path::new("/sys/block"));
    let Ok(entries) = std::fs::read_dir("/sys/block") else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(dev) = name.to_str() else { continue };
        if !dev.starts_with("vd") {
            continue;
        }
        let value = if swap_dev.as_deref() == Some(dev) {
            SWAP_READ_AHEAD_KB
        } else {
            READ_AHEAD_KB
        };
        apply_one(&entry.path().join("queue/read_ahead_kb"), dev, value);
    }
}

fn apply_one(path: &Path, dev: &str, value: &str) {
    match std::fs::write(path, value) {
        Ok(()) => tracing::info!(dev, read_ahead_kb = value, "block readahead set"),
        Err(error) => tracing::warn!(dev, %error, "block readahead not applied"),
    }
}
