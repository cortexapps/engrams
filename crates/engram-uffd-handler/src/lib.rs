//! Userfaultfd page-fault handler for Firecracker UFFD-backed
//! snapshot restore.
//!
//! ADR 0020 Route B chunk-native shape — the handler reads **no**
//! `memory.bin` file and holds no mmap. It consults the canonical +
//! per-session memory manifests and serves every fault from the chunk
//! cache/store: a session-divergent hash, the canonical chunk hash at
//! that offset, or a zero page (`UFFDIO_ZEROPAGE`) for offsets the
//! manifest omits.
//!
//! Usage from the host:
//!
//! ```text
//!   engram-uffd-handler \
//!     --listen /tmp/uffd-handler.sock \
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
//! - [`chunked`] — data-plane resolver: per-fault, "which chunk hash
//!   backs this offset (or is it zero-fill)?" Pure Rust,
//!   unit-testable without a UFFD.
//! - `runtime` (Linux-only) — UFFD event loop that consumes a
//!   [`chunked::ChunkedMemoryBackend`] and serves faults via
//!   `UFFDIO_COPY` / `UFFDIO_ZEROPAGE`.

pub mod chunked;
pub mod proto;
pub mod working_set;

#[cfg(target_os = "linux")]
pub mod base_shm;
#[cfg(target_os = "linux")]
pub mod runtime;

pub use chunked::{ChunkedBackendError, ChunkedMemoryBackend, ResolvedPage};
pub use proto::{GuestRegionUffdMapping, HANDSHAKE_BUF_BYTES};
pub use working_set::WorkingSetRecorder;

#[cfg(target_os = "linux")]
pub use runtime::{recv_handshake, run_listener, HandlerError, Runtime};
