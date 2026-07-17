//! The host-internal deterministic simulator (ADR 0098 Phase 2, P2).
//!
//! `engram-dst-host` is the sibling of `engram-dst`: where that crate drives
//! the real coordinator drivers over `engram-sim`, this one drives the
//! PORTABLE host-agent machinery — `ChunkedDiskBackend` and the shutdown
//! spool — over `engram-host-core`'s [`HostEffects`](engram_host_core::HostEffects)
//! seams, on a per-run [`SimFs`] tempdir. It clones `engram-dst`'s
//! determinism discipline exactly: a seeded `ChaCha8` pick stream forked
//! from the world's `SimEntropy`, a current-thread paused-tokio runtime,
//! run-step-to-completion scheduling, BTreeMap-ordered decisions, a
//! [`SimReport`] the replay-diff test compares, and no banned time/entropy
//! calls (see `clippy.toml`).
//!
//! **Hard boundary:** this crate does NOT depend on `engram-coordinator`.
//! The coordinator and host simulators sit in disjoint cargo-dep closures so
//! the CI detector runs each lane independently.
//!
//! # The step/oracle surface (P2)
//!
//! Steps: [`GuestWrite`](Step::GuestWrite) / [`GuestRead`](Step::GuestRead)
//! (content-tag-stamped synthetic chunks + read-after-write),
//! [`FlushTick`](Step::FlushTick), [`SpoolExport`](Step::SpoolExport) /
//! [`SpoolAdopt`](Step::SpoolAdopt), [`CrashProcess`](Step::CrashProcess) /
//! [`Restart`](Step::Restart), [`AdvanceTime`](Step::AdvanceTime). The single
//! oracle is acked-write durability (see [`invariants`]).
//!
//! # Deferred to P3+
//!
//! Faults (the coord stub's adversarial [`ScriptedResponse`] queue is wired
//! but never fired), the lifecycle-flow extractions (Flow A–E), and
//! scheduler-driven crash-point INJECTION (the [`CrashPoint`] boundaries are
//! named + coverage-tested here, but cutting the process at one is P4).

pub mod coord_stub;
pub mod effects;
pub mod invariants;
pub mod scheduler;
pub mod simfs;
pub mod world;

pub use coord_stub::{RecordedPublish, ScriptedResponse, SimCoordClient};
pub use effects::{sim_effects, SeamEvent, SeamLog, SimDeviceSync, SimNbd};
pub use invariants::Violation;
pub use scheduler::{Profile, Sim, SimReport, Step, NUM_SANDBOXES};
pub use simfs::{CrashPoint, SimFs};
pub use world::{AckedWriteLedger, LedgerEntry, SandboxSlot, SimHost, CHUNK_SIZE, NUM_CHUNKS};
