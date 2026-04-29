//! Engram core: shared types, trait surfaces, and error enums.
//!
//! This crate intentionally does no I/O. Implementations live in sibling
//! crates (`engram-postgres`, `engram-cloud-*`, `engram-sandbox-firecracker`,
//! `engram-sandbox-process`) and are wired together by the binary crates
//! (`engram-coordinator`, `engram-host-agent`).

pub mod error;
pub mod traits;
pub mod types;

pub use error::{BackendError, MetaError, SandboxError, SecretError};
pub use traits::{CloudBackend, MetadataStore, SandboxBackend, SecretStore};
pub use types::*;
