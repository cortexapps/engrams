//! Trait surfaces that implementation crates fill in.
//!
//! Pluggability seams:
//! - [`CloudBackend`] — preemption signals, host metadata, optional autoscaling.
//! - [`MetadataStore`] — Postgres-backed authoritative session/host/snapshot index.
//! - [`SandboxBackend`] — VM lifecycle (create/exec/snapshot/restore/destroy).
//! - [`SecretStore`] — pluggable secret resolution.
//! - [`BlobStorage`] — cold-tier snapshot storage (S3/GCS/local fs).
//!   Reintroduced in Phase 6 / ADR 0005 as the disk-pressure flush
//!   target — see `docs/adr/0005-disk-pressure-blob-tier.md`.

pub mod cloud;
pub mod metadata;
pub mod sandbox;
pub mod secrets;
pub mod storage;

pub use cloud::{CloudBackend, PreemptionStream};
pub use metadata::MetadataStore;
pub use sandbox::{HarnessByteStream, HarnessDial, HarnessSink, SandboxBackend};
pub use secrets::{ResolvedSecret, SecretBundle, SecretContext, SecretStore};
pub use storage::{BlobObjectMeta, BlobStorage, ByteStream};
