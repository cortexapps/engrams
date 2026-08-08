//! The block-device sync seam (ADR 0098 Phase 2).
//!
//! [`DeviceSync`] abstracts the one host-page-cache sync the shutdown/
//! checkpoint paths force on `/dev/nbdN` before draining: the 2026-07-16
//! session-85e0298a RCA showed FC's buffered (`cache_type=Unsafe`) drive
//! can leave guest-acked writes sitting in the host page cache for the NBD
//! device — a tier the dirty-map flush never sees — so we `sync_all()` the
//! device fd down into the daemon's dirty tier while the serve loop is
//! still alive to ack the writeback.
//!
//! **Decision — the trait carries ONLY `sync_device`.** The natural
//! companion, "abandon the device in place for the successor generation,"
//! is `NbdSandboxState::abandon_for_shutdown`, which *consumes* `self`
//! (drops the scheduler, `mem::forget`s the slot lease, drops the backend
//! Arc — see `disk_daemon::runtime`). Abandonment is inseparable from
//! owning the whole `NbdSandboxState`; modelling it as a `&self` trait
//! method would be a lie about that ownership transfer. So abandon stays a
//! method on the state, and `DeviceSync` is a single-method seam. The prod
//! impl lives in `engram-host-agent`; the disk daemon's capture primitive
//! (`ChunkedDiskBackend::sync_host_device`, called by `flush_local` before
//! every freeze) is the main caller. The sim impl records the sync against
//! its acked-write ledger.

use std::io;
use std::path::Path;

use async_trait::async_trait;

/// Force the host page cache for a block device down to its backing store.
#[async_trait]
pub trait DeviceSync: Send + Sync {
    /// `sync_all()` the device at `path` (opened read+write). Capture
    /// paths treat a failure as a hard error (an unsynced freeze can
    /// publish a torn chunk); best-effort warm-up paths warn and
    /// proceed — pages left behind ride the kernel's dead-conn parking
    /// to the successor.
    async fn sync_device(&self, path: &Path) -> io::Result<()>;
}
