//! The block-device sync seam (ADR 0098 Phase 2).
//!
//! [`DeviceSync`] is the portable block-device sync effect used by the host
//! simulator.
//!
//! **Decision — the trait carries ONLY `sync_device`.** The natural
//! companion, "abandon the device in place for the successor generation,"
//! is `NbdSandboxState::abandon_for_shutdown`, which *consumes* `self`
//! (drops the scheduler, `mem::forget`s the slot lease, drops the backend
//! Arc — see `disk_daemon::runtime`). Abandonment is inseparable from
//! owning the whole `NbdSandboxState`; modelling it as a `&self` trait
//! method would be a lie about that ownership transfer. So abandon stays a
//! method on the state, and `DeviceSync` is a single-method seam.

use std::io;
use std::path::Path;

use async_trait::async_trait;

/// Force the host page cache for a block device down to its backing store.
#[async_trait]
pub trait DeviceSync: Send + Sync {
    /// `sync_all()` the device at `path` (opened read+write). A failure is
    /// a `warn`-and-proceed at the call site, not a hard error — pages left
    /// behind ride the kernel's dead-conn parking to the successor.
    async fn sync_device(&self, path: &Path) -> io::Result<()>;
}
