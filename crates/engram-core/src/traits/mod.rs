//! Trait surfaces that implementation crates fill in.
//!
//! These define the four pluggability seams described in `DESIGN.md`:
//! - [`CloudBackend`] — preemption signals, host metadata, optional autoscaling.
//! - [`BlobStorage`] — durable cold-tier object store for snapshots and images.
//! - [`MetadataStore`] — Postgres-backed authoritative session/host/snapshot index.
//! - [`SandboxBackend`] — VM lifecycle (create/exec/snapshot/restore/destroy).

pub mod cloud;
pub mod metadata;
pub mod sandbox;
pub mod secrets;
pub mod storage;

pub use cloud::{CloudBackend, PreemptionStream};
pub use metadata::MetadataStore;
pub use sandbox::SandboxBackend;
pub use secrets::{ResolvedSecret, SecretBundle, SecretContext, SecretStore};
pub use storage::{BlobStorage, ByteStream, ObjectMetadata};
