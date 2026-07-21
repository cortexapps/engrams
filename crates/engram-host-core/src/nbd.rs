//! The NBD-kernel seam (ADR 0098 Phase 2, Flow B).
//!
//! [`NbdKernel`] captures the four kernel operations the attach/reattach
//! flows (`attach_backend`, `reattach_manifest`) drive: `connect`
//! (`NBD_CMD_CONNECT`), `reconfigure` (`NBD_CMD_RECONFIGURE` — the
//! survivor-rehydrate primitive), `disconnect` (`NBD_CMD_DISCONNECT`), and
//! `backend_identifier` (the `/sys/block/nbdN/backend` read). The Linux
//! prod impl (in `engram-host-agent`, cfg `target_os = "linux"`) wraps
//! `disk_daemon::nbd_netlink` + sysfs; the sim impl models a device
//! registry with a single-owner invariant and the dead-conn window.
//!
//! **P1 scope**: this trait + its prod impl are DEFINED here; the call
//! sites in `disk_daemon::runtime` (`reattach_manifest`/`attach_backend`)
//! are NOT rewired onto it — threading the seam through their signatures
//! (they pass a raw serve-socket fd the kernel dups, plus geometry) is
//! Flow B extraction, which the ADR assigns to P7. Defining the shape now
//! lets P7 rewire without re-litigating the seam.
//!
//! The serve-socket fd is modelled as a plain `i32` (a `RawFd` on the prod
//! side) so this crate stays portable — the sim ignores it.

use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

/// One `/dev/nbdN` device the kernel currently has CONNECTED — the Layer-2
/// kernel-derived rehydrate inventory (ADR 0098 §Phase 3, Wave 7b, #784). Read
/// from sysfs ground truth, not from any tracked record: a survivor whose
/// records were lost still appears here as long as its kernel binding survives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectedDevice {
    /// The device node (`/dev/nbdN`).
    pub device: PathBuf,
    /// The configuring owner pid the kernel records (`/sys/block/nbdN/pid`). The
    /// stale-binding sweep probes this against liveness (self / alive / dead).
    pub owner_pid: i32,
    /// The identifier the kernel recorded at CONNECT (`/sys/block/nbdN/backend`,
    /// the P7 backend identifier), if present — the corroborating reconcile key
    /// alongside the device path. `None` for a pre-identifier kernel or an
    /// unreadable attr.
    pub backend_id: Option<String>,
}

/// `NBD_CMD_CONNECT` parameters: configure a fresh `/dev/nbdN` to serve
/// from `serve_fd` with the given geometry and the stable
/// `backend_identifier` the kernel records for later RECONFIGURE checks.
pub struct NbdConnectRequest<'a> {
    pub device: &'a Path,
    /// Kernel-side half of the serve socketpair (a `RawFd`); the kernel
    /// dups it.
    pub serve_fd: i32,
    pub size_bytes: u64,
    pub block_size: u64,
    /// Per-request timeout while a connection is live.
    pub timeout_secs: u64,
    /// How long queued I/O survives with no live connection before
    /// failing — the pod-roll grace window.
    pub dead_conn_timeout_secs: u64,
    pub backend_identifier: &'a str,
}

/// `NBD_CMD_RECONFIGURE` parameters: hand a NEW serve socket to an
/// already-configured device whose previous connection died. Requires a
/// `backend_identifier` matching the one recorded at CONNECT.
pub struct NbdReconfigureRequest<'a> {
    pub device: &'a Path,
    pub serve_fd: i32,
    pub timeout_secs: u64,
    pub dead_conn_timeout_secs: u64,
    pub backend_identifier: &'a str,
}

/// The kernel NBD control-plane operations behind the attach/reattach
/// flows.
#[async_trait]
pub trait NbdKernel: Send + Sync {
    /// `NBD_CMD_CONNECT`: configure `device` to serve from the request's
    /// socket. Fails `EBUSY` if the device already has a config.
    async fn connect(&self, req: NbdConnectRequest<'_>) -> io::Result<()>;

    /// `NBD_CMD_RECONFIGURE`: replace the dead connection slot on an
    /// already-configured `device`, requeueing I/O parked under
    /// `dead_conn_timeout`.
    async fn reconfigure(&self, req: NbdReconfigureRequest<'_>) -> io::Result<()>;

    /// `NBD_CMD_DISCONNECT`: tear the device's config down. Works on a
    /// device whose configuring process is long dead.
    async fn disconnect(&self, device: &Path) -> io::Result<()>;

    /// The identifier the kernel recorded at CONNECT —
    /// `/sys/block/nbdN/backend`. `None` when the attr is missing/
    /// unreadable (device never netlink-configured, or pre-identifier
    /// kernel).
    fn backend_identifier(&self, device: &Path) -> Option<String>;

    /// Enumerate every `/dev/nbdN` the kernel currently has CONNECTED — those
    /// with a populated `/sys/block/nbdN/pid` — as the Layer-2 kernel-derived
    /// rehydrate inventory (ADR 0098 §Phase 3, Wave 7b, #784). This is GROUND
    /// TRUTH: the startup reconcile classifies tracked records AGAINST this list,
    /// never the reverse, so a survivor whose records were lost is still seen
    /// (and quarantined) rather than silently skipped. Deterministically ordered
    /// (device-ordinal ascending). A cold-path sysfs scan, called once at
    /// register time.
    fn connected_devices(&self) -> Vec<ConnectedDevice>;
}
