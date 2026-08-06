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

/// Set `read_ahead_kb` on every virtio block device. Best-effort by
/// design: a missing sysfs tree (non-Linux backends, tests) or an
/// unwritable file logs and moves on — tuning must never block agent
/// bring-up.
pub fn apply_block_readahead() {
    let Ok(entries) = std::fs::read_dir("/sys/block") else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(dev) = name.to_str() else { continue };
        if !dev.starts_with("vd") {
            continue;
        }
        apply_one(&entry.path().join("queue/read_ahead_kb"), dev);
    }
}

fn apply_one(path: &Path, dev: &str) {
    match std::fs::write(path, READ_AHEAD_KB) {
        Ok(()) => tracing::info!(dev, read_ahead_kb = READ_AHEAD_KB, "block readahead set"),
        Err(error) => tracing::warn!(dev, %error, "block readahead not applied"),
    }
}
