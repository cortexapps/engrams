//! Userfaultfd page-fault handler for Firecracker UFFD-backed snapshot
//! restore.
//!
//! Usage from the host:
//!
//! ```text
//!   engram-uffd-handler --memory-bin /snap/memory.bin \
//!                       --listen /tmp/uffd-handler.sock
//! ```
//!
//! Then `PUT /snapshot/load` with
//! `mem_backend = { backend_type: "Uffd", backend_path: "/tmp/uffd-handler.sock" }`
//! — Firecracker connects to the listener, sends the UFFD over
//! SCM_RIGHTS plus the memory layout as JSON, and disconnects. The
//! handler stays running for the lifetime of the VM, serving page
//! faults from `memory.bin` on demand.
//!
//! See [`proto`] for the wire format and [`runtime`] for the loop.

pub mod proto;

#[cfg(target_os = "linux")]
pub mod runtime;

pub use proto::{GuestRegionUffdMapping, HANDSHAKE_BUF_BYTES};

#[cfg(target_os = "linux")]
pub use runtime::{recv_handshake, run_listener, HandlerError, Runtime};
