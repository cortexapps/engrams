//! ADR 0014 M1.10: warm-pool memory-cost validation scaffold.
//!
//! The warm-pool depth economics rest on a load-bearing claim:
//! N concurrent microVMs restored from one canonical snapshot
//! share the canonical memory.bin mmap via the kernel page cache,
//! so N warm slots × 2 GiB doesn't cost N × 2 GiB resident RAM.
//!
//! This test would measure that claim directly by sampling
//! `/proc/meminfo` MemAvailable across N concurrent restores. It
//! is **currently disabled** with `#[ignore]` AND an early return
//! that short-circuits before driving FC, because the concurrent-
//! restore path it would exercise isn't operational yet:
//!
//!   - FC's `state.bin` embeds the source-sandbox-id-keyed vsock
//!     UDS path. Two FCs restoring from one snapshot collide on
//!     `bind(2)` against that same path.
//!   - ADR 0014 calls out per-FC mount-namespace + bind-mount as
//!     the unblocker. Once that lands and `CEILING_TARGET` in
//!     `engram-host-agent/src/warm_pool.rs` rises above 1, remove
//!     the early-return scaffold below and the test will measure
//!     the actual sharing claim against `N=5` concurrent VMs.
//!
//! Keeping the scaffold + comments in place so the unblock work
//! has a clear pre-built validation surface.
//!
//! Run on the Linux dev VM:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-host-agent --test warm_pool_memory \
//!     -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

use std::path::PathBuf;

/// How many concurrent restores to attempt once concurrent-restore
/// is unblocked.
const _N: usize = 5;

/// Per-VM advertised RAM size in MiB. The eventual test asserts
/// the per-slot resident delta is well below this — that's the
/// page-cache sharing signal.
const _PER_VM_MIB: u32 = 128;

/// Fraction of `_PER_VM_MIB` tolerated as per-slot resident growth
/// before flagging the sharing claim as broken. 50% leaves
/// generous slack for stack + page-table + kernel-side per-process
/// state.
const _PER_SLOT_RESIDENT_FRACTION: f64 = 0.5;

#[tokio::test]
#[ignore = "blocked on per-FC mount-namespace work; gated on CEILING_TARGET > 1"]
async fn n_restores_share_canonical_mmap_via_page_cache() {
    // Early return: until ADR 0014's per-FC mount-namespace work
    // lands and CEILING_TARGET rises above 1, exercising N
    // concurrent restores would hit a vsock UDS bind collision on
    // the 2nd restore (FC state.bin embeds the source-keyed path).
    // The full body of this test lives in git history at the
    // commit that lands the mount-namespace unblock; un-stub it
    // there.
    eprintln!(
        "SKIP: warm_pool_memory blocked on per-FC mount-namespace work \
         (concurrent-restore vsock UDS bind collision). See \
         CEILING_TARGET in engram-host-agent/src/warm_pool.rs."
    );
    let _ = preflight();
}

#[allow(dead_code)]
fn preflight() -> Option<(PathBuf, PathBuf)> {
    let kernel = std::env::var("FC_TEST_KERNEL").ok().map(PathBuf::from)?;
    let rootfs = std::env::var("FC_TEST_ROOTFS").ok().map(PathBuf::from)?;
    if !std::path::Path::new("/dev/kvm").exists() {
        eprintln!("SKIP: /dev/kvm not present");
        return None;
    }
    Some((kernel, rootfs))
}

/// Parse `/proc/meminfo` and return MemAvailable in KiB. Kept here
/// so the un-stub PR (when mount-namespacing lands) doesn't have
/// to re-derive it.
#[allow(dead_code)]
fn mem_available_kib() -> std::io::Result<u64> {
    let text = std::fs::read_to_string("/proc/meminfo")?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let val: u64 = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "MemAvailable parse failed",
                    )
                })?;
            return Ok(val);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "MemAvailable line not in /proc/meminfo",
    ))
}
