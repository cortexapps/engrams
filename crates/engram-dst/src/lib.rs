//! The deterministic simulator (ADR 0098 D5).
//!
//! A seeded step scheduler drives the REAL coordinator drivers
//! (`queue_scanner::run_once`, `reconcile_with_deps`,
//! `dead_host::run_once`, `session_ops::drive_session`) over
//! `engram-sim`'s substrate: N stateless replicas sharing one
//! `SimMetadataStore` (== Postgres, the single authority) and M simulated
//! hosts behind per-host `HostClient` fakes. Time is tokio's paused
//! clock; every random choice comes from a `ChaCha8Rng` forked into
//! per-component streams, so a failure replays exactly from its seed.

pub mod invariants;
pub mod scheduler;
pub mod workload;
pub mod world;

pub use scheduler::{DriverKind, Profile, Sim, SimReport, Step};
pub use world::SimWorld;
