//! Boot-time block-device tuning for the chunked-NBD rootfs.
//!
//! The guest's virtio disks are backed by the host disk daemon, where one
//! read round trip costs ~100–200 µs regardless of size — while bytes
//! inside an already-served range are nearly free. The kernel default
//! readahead (128 KiB) is sized for local disks where speculative reads
//! have real device cost; here a bigger window converts long chains of
//! small dependent reads (JVM classloading is the measured worst case:
//! ~47k reads at qd≈1 on a dev-brain first `gradlew help`) into fewer,
//! larger round trips. Measured on the profiling ladder
//! (engrams-internal #131): 128 KiB → 4 MiB readahead cut a guest-cold
//! `gradlew help` from 5.2 s to 1.7 s.
//!
//! Applied once at agentd startup. A fresh boot captures the setting
//! into the base snapshot, so every session restored from that snapshot
//! inherits it (sysfs state rides the VM memory image). Sessions
//! restored from snapshots captured before this change keep the old
//! default until their image re-captures — agentd is baked per-image, so
//! a restored VM runs the agentd that booted it.

use std::path::Path;

/// 4 MiB — the measured knee of the readahead sweep. Big enough to
/// amortize round trips, small enough that speculative readahead stays
/// well under one 16 MiB chunk and cannot drag in neighbouring chunks.
const READ_AHEAD_KB: &str = "4096";

/// ADR 0112: the swap device's readahead. Swap-in is random 4 KiB
/// against fast host-file backing — the 4 MiB window is sized for the
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
