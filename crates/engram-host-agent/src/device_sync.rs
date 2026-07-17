//! The production [`DeviceSync`] impl (ADR 0098 Phase 2, Flow A).
//!
//! [`HostDeviceSync`] is the prod side of `engram-host-core`'s
//! [`DeviceSync`] seam: it opens `/dev/nbdN` read+write and `sync_all()`s it,
//! forcing the host page cache for the device down into the daemon's dirty
//! tier while the serve loop is still alive to ack the writeback. It lives
//! here, next to its only caller (the SIGTERM final-flush pass in
//! `pooled_backend`), while the trait and the simulator's recording stub live
//! in the portable crates.
//!
//! # The O_DIRECT rider (2026-07-16 session-85e0298a RCA) — verdict: no-op
//!
//! The RCA found FC's drive is buffered host I/O (`cache_type=Unsafe`), so
//! guest-acked writes can sit in the HOST page cache for `/dev/nbdN` — a tier
//! the dirty-map flush never sees, and one the pod-handoff dead-connection
//! window can silently drop (`lost async page write`). ADR 0098's known-bugs
//! list floated "attempt **O_DIRECT** for the NBD device sync path" as a P4
//! hardening. Researching the kernel semantics first (AGENTS.md
//! research-over-guess), **O_DIRECT on this fd changes nothing** and is the
//! wrong lever:
//!
//! 1. All opens of the same block-device special file share ONE page cache —
//!    the block device's `bdev` inode `address_space`. FC's buffered writes
//!    dirty pages in that shared cache; a `sync_all()` (= `fsync`) issued
//!    through *any* fd on `/dev/nbdN` writes back the entire shared cache
//!    (`blkdev_fsync` → `filemap_write_and_wait` over the bdev mapping, then
//!    a device flush). So the buffered `sync_all` already flushes FC's
//!    dirty pages — that is why this path exists and works.
//! 2. `O_DIRECT` only governs the data path of `read()`/`write()` on the
//!    opening fd; it does not change `fsync`/`sync_all` semantics, and it
//!    does not invalidate or coordinate with page-cache pages dirtied by
//!    another fd. This fd is **sync-only** (we never read or write through
//!    it — only `sync_all`), so `O_DIRECT` has literally nothing to act on:
//!    it would be pure cargo-cult and would add the real block-alignment
//!    fragility (an `O_DIRECT` open can `EINVAL` on some configs) for zero
//!    change in the flushed bytes.
//!
//! So we keep the buffered open + `sync_all` — the correct primitive — and do
//! not change the sync path (no FC regression test is warranted, because no
//! kernel behavior changed). The residual read-side risk the RCA names (a
//! successor reading through a device whose page cache holds stale/dropped
//! pages) is a different mechanism: cross-tenant slot reuse is already closed
//! by the `BLKFLSBUF` invalidate-on-CONNECT (`disk_daemon::runtime`), and the
//! post-RECONFIGURE **verify-on-read** that observes the seeded acked bytes is
//! ADR 0098's documented fallback rider — it rides **P7** (Flow B), not P4.
//! The P4 ADR row records this verdict.

use std::io;
use std::path::Path;

use async_trait::async_trait;
use engram_host_core::DeviceSync;

/// The production block-device sync: `OpenOptions::new().read(true)
/// .write(true).open(dev)?` then `sync_all()`, off the async runtime via
/// `spawn_blocking` (the open + fsync are blocking syscalls). A join failure
/// maps to an `io::Error` the caller warn-and-proceeds on — pages left behind
/// ride the kernel's dead-conn parking to the successor.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostDeviceSync;

#[async_trait]
impl DeviceSync for HostDeviceSync {
    async fn sync_device(&self, path: &Path) -> io::Result<()> {
        let dev = path.to_path_buf();
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&dev)?;
            f.sync_all()
        })
        .await
        .map_err(|e| io::Error::other(format!("device sync task join: {e}")))?
    }
}
