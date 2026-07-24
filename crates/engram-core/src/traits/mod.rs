//! Trait surfaces that implementation crates fill in.
//!
//! Pluggability seams:
//! - [`CloudBackend`] — host metadata, optional autoscaling.
//! - [`MetadataStore`] — Postgres-backed authoritative session/host/snapshot index.
//! - [`SandboxBackend`] — local VMM driver (Firecracker / VZ / Process).
//! - [`HostClient`] — the coord↔host boundary; one impl wraps a local
//!   `SandboxBackend` + `HarnessHub`, the other dispatches over WS.
//! - [`SecretStore`] — pluggable secret resolution.
//! - [`Integration`] — provider integration seam: capability-scoped credential
//!   mint + a hybrid mediated action (ADR 0056; subsumes the retired
//!   `GitForge`).
//! - [`BlobStorage`] — cold-tier snapshot storage (S3/GCS/local fs).
//!   Reintroduced in Phase 6 / ADR 0005 as the disk-pressure flush
//!   target — see `docs/adr/0005-disk-pressure-blob-tier.md`.

pub mod clock;
pub mod cloud;
pub mod host_client;
pub mod integration;
pub mod metadata;
pub mod sandbox;
pub mod secrets;
pub mod storage;

pub use clock::{Clock, Entropy, OsEntropy, SystemClock};
pub use cloud::CloudBackend;
pub use host_client::{HostClient, SessionFence};
pub use integration::{
    default_inject_header, CredentialHint, InjectHeader, Integration, MintFieldKind,
    MintFieldSchema, MintKindDescriptor, ResolvedFields, ScopedCredential,
};
pub use metadata::{
    CreateDisposition, DisableEnabledImageOutcome, ExecLifecycleEventKind, ExecOutputStream,
    GcCandidateRow, MetadataStore, PlacementNoFit, SessionCreateWriteSet, SnapshotTotals,
    UpdateOutcome,
};
pub use sandbox::{
    AgentRefresh, BrowserStart, ForgeSink, HarnessByteStream, HarnessDial, HarnessSink,
    SandboxBackend, UploadSink,
};
pub use secrets::{
    LayeredSecretStore, ResolvedSecret, SecretBundle, SecretContext, SecretStore, StaticSecretStore,
};
pub use storage::{BlobObjectMeta, BlobStorage, ByteStream};
