//! Content-addressed chunked-immutable storage for Engram.
//!
//! ADR 0007: sandbox disk and memory state live as **chunks** in a
//! `BlobStorage` backend. Each chunk is content-addressed by sha256
//! and immutable. A **manifest** is the durable, versioned view of
//! a virtual disk or memory image: a list of `(offset, chunk_hash)`
//! pairs.
//!
//! # Why this shape
//!
//! - **Cross-VM dedup**: chunks identical across sessions hash the
//!   same and store once. Two sessions running the same base image
//!   share its chunks at the storage layer for free.
//! - **Copy-on-write at three layers**: disk (manifest refs), memory
//!   (MAP_PRIVATE of canonical), session-fork (manifest copy is ~KB).
//! - **Fast cross-host migration**: a host doesn't need to copy state
//!   on migration; it just adopts a manifest and pages chunks in as
//!   the VM touches them.
//! - **Spot/preemption viable**: per-snapshot flush is "PUT only the
//!   chunks that changed since the last manifest version" — fits in
//!   30s windows even for many concurrent sessions.
//!
//! # Layout
//!
//! Storage keys in the underlying `BlobStorage`:
//!
//! ```text
//! chunks/sha256/<2-hex>/<rest-of-64-hex>      (immutable, content-addressed)
//! manifests/<manifest_id>/v<version>.json     (immutable, versioned)
//! traces/<manifest_id>/<host_id>.json         (host-local working-set hint)
//! ```
//!
//! # Layers above this crate
//!
//! - `engram-host-agent::disk_daemon` — NBD daemon serving chunked
//!   manifests as Linux block devices (FC backend)
//! - `engram-uffd-handler` — UFFD page-fault handler that resolves
//!   guest memory faults to memory-manifest chunks (FC backend)
//! - `engram-sandbox-vz::disk` — materialize-to-file for macOS dev
//! - `engram-rootfs-materializer` — chunks bake/materialize outputs

pub mod bootstrap;
pub mod budget;
pub mod cache;
pub mod error;
pub mod file;
pub mod gc;
pub mod manifest;
pub mod reader;
pub mod region;
pub mod resolver;
pub mod snapshot_blob;
pub mod store;
pub mod working_set;

pub use bootstrap::{Bootstrap, BootstrapEntry, BOOTSTRAP_SCHEMA_VERSION};
pub use budget::UploadBudget;
pub use cache::{ChunkCache, ChunkCacheConfig};
pub use error::{ChunkStoreError, Result};
pub use file::ChunkFileStats;
pub use gc::{GcError, PinSet, DEFAULT_COLLECT_CONCURRENCY};
pub use manifest::{
    ChunkHash, ChunkRef, ChunkSize, Manifest, ManifestKind, ManifestRef, DEFAULT_DISK_CHUNK_SIZE,
    DEFAULT_MEMORY_CHUNK_SIZE,
};
pub use resolver::{BlobStorageResolver, ChunkResolver, TieredChunkResolver};
pub use store::ChunkStore;
pub use working_set::{TraceRef, WorkingSetTrace};
