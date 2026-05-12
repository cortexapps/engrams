//! Userfaultfd page-fault handler for Firecracker UFFD-backed
//! snapshot restore.
//!
//! ADR 0007 chunked-memory shape — the handler no longer reads a
//! single `memory.bin` file. It mmaps the **canonical** memory
//! snapshot (one file per image on this host, shared via the host
//! page cache across every session of that image) and consults a
//! per-session memory manifest to decide whether each fault serves
//! from the canonical mmap or from a session-divergent chunk in
//! the chunk store.
//!
//! Usage from the host:
//!
//! ```text
//!   engram-uffd-handler \
//!     --listen /tmp/uffd-handler.sock \
//!     --canonical-memory /var/lib/engram/canonical/<image-id>.bin \
//!     --canonical-manifest <uuid>:<version> \
//!     --session-manifest   <uuid>:<version>
//!
//!   # Blob backend selected via env:
//!   ENGRAM_BLOB_BACKEND=gcs ENGRAM_GCS_BUCKET=engram-chunks
//! ```
//!
//! Then Firecracker's `PUT /snapshot/load` with
//! `mem_backend = { backend_type: "Uffd", backend_path: "..." }`
//! connects to the socket, hands over the userfaultfd + a JSON
//! description of the guest's memory regions, and the handler
//! enters its fault loop for the VM's lifetime.
//!
//! Layout of this crate:
//!
//! - [`proto`] — Firecracker handshake wire format.
//! - [`chunked`] — data-plane resolver: per-fault, "is this page
//!   in canonical or do we need to fetch?" Pure Rust, unit-testable
//!   without a UFFD.
//! - `runtime` (Linux-only) — UFFD event loop that consumes a
//!   [`chunked::ChunkedMemoryBackend`] and serves faults via
//!   `UFFDIO_COPY`.

pub mod chunked;
pub mod proto;
pub mod working_set;

#[cfg(target_os = "linux")]
pub mod runtime;

pub use chunked::{ChunkedBackendError, ChunkedMemoryBackend, ResolvedPage};
pub use proto::{GuestRegionUffdMapping, HANDSHAKE_BUF_BYTES};
pub use working_set::WorkingSetRecorder;

#[cfg(target_os = "linux")]
pub use runtime::{recv_handshake, run_listener, HandlerError, Runtime};
