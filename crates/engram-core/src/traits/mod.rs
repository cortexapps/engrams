//! Trait surfaces that implementation crates fill in.
//!
//! Pluggability seams:
//! - [`CloudBackend`] — preemption signals, host metadata, optional autoscaling.
//! - [`MetadataStore`] — Postgres-backed authoritative session/host/snapshot index.
//! - [`SandboxBackend`] — VM lifecycle (create/exec/snapshot/restore/destroy).
//! - [`SecretStore`] — pluggable secret resolution.
//!
//! `BlobStorage` was removed in Phase 4 along with snapshot replication —
//! durability for sessions moved to git checkpoints. Image distribution
//! (Phase 5) will use a Docker registry.

pub mod cloud;
pub mod metadata;
pub mod sandbox;
pub mod secrets;

pub use cloud::{CloudBackend, PreemptionStream};
pub use metadata::MetadataStore;
pub use sandbox::SandboxBackend;
pub use secrets::{ResolvedSecret, SecretBundle, SecretContext, SecretStore};
