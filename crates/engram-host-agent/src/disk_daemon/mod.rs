//! NBD-backed virtual block device for FC disks.
//!
//! ADR 0007 Phase 4. The host-agent spawns one instance of this
//! daemon per FC sandbox. It serves a chunked disk manifest as an
//! NBD endpoint; we bind that endpoint to a kernel `/dev/nbdN`
//! device via the `NBD_SET_SOCK` ioctl family, and Firecracker
//! attaches `/dev/nbdN` as a virtio-blk drive — same `path_on_host`
//! shape the legacy ext4-file path used, but the bytes come from
//! the chunk store on demand.
//!
//! What this buys us over the legacy "materialize chunked manifest
//! to a single ext4 file then attach" path:
//!
//! - **Stream-on-demand reads.** Cold start of a 16 GiB image costs
//!   one chunk fetch per accessed region, not "download the whole
//!   thing then start the VM." Production parity with Replit /
//!   Modal style sandboxing.
//! - **Per-session write isolation.** Writes hit the daemon's
//!   in-memory dirty-chunk buffer; the base chunks in the chunk
//!   store stay immutable. Two sessions of the same image share
//!   the base chunks via the local NVMe cache and only diverge in
//!   their respective daemons.
//! - **Snapshot-as-flush.** Snapshot is "hash dirty chunks → PUT
//!   to the chunk store → produce new manifest version" — no
//!   tar/zstd compression pass, no big single-blob upload.
//!
//! Module shape:
//!
//! - [`nbd`] — wire format codec (`Request` + `Reply`). Pure Rust,
//!   target-agnostic, unit-testable on macOS dev.
//! - [`backend`] — `ChunkedDiskBackend` data plane: maps NBD offset/
//!   length to chunk operations. Pure Rust; the per-chunk
//!   read-fetch / write-into-dirty-buffer logic lives here.
//! - `runtime` (Linux-only) — the NBD server loop + the kernel
//!   `/dev/nbdN` orchestration (loaded on demand via
//!   `engram_host_agent::disk_daemon::runtime::spawn`).

pub mod backend;
pub mod flush_scheduler;
pub mod live_manifest_publisher;
pub mod nbd;
pub mod slot;
pub mod spool;

#[cfg(target_os = "linux")]
pub mod nbd_netlink;
#[cfg(target_os = "linux")]
pub mod runtime;

pub use backend::{
    ChunkedDiskBackend, DiskBackendError, DiskFlushOutcome, PendingDiskFlush, PostCopyDiskFetcher,
    PostCopyDiskSeal, PostCopyDrainSubscription, DEFAULT_DIRTY_THRESHOLD_BYTES,
};
pub use flush_scheduler::{
    FlushScheduler, FlushSchedulerConfig, FlushSchedulerHandle, LiveManifestPublisher,
    NoOpLiveManifestPublisher,
};
pub use live_manifest_publisher::{
    CoordLiveManifestPublisher, LiveManifestPublisherHandle, SessionResolver,
};
pub use nbd::{NbdCommand, NbdReply, NbdRequest, NbdWireError, NBD_REPLY_MAGIC, NBD_REQUEST_MAGIC};
pub use slot::{build_from_kernel as build_nbd_pool_from_kernel, NbdSlot, NbdSlotAllocator};

#[cfg(target_os = "linux")]
pub use runtime::{
    attach_manifest, attach_manifest_content, flush_block_device_cache, reattach,
    reattach_manifest, recover_stuck_nbd_devices, spawn, NbdHandle, NbdRuntimeError,
    NbdSandboxState, NBD_BLOCK_SIZE,
};
