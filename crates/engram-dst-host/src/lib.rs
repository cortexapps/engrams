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
//! # P3 (Flow C — reconcile)
//!
//! P3 adds [`ReconcileTick`](Step::ReconcileTick) (+ the
//! [`DropLocalBinding`](Step::DropLocalBinding) /
//! [`RevokeOwnership`](Step::RevokeOwnership) perturbations), driving the REAL
//! [`reconcile_once`](engram_host_agent::teardown_reconcile::reconcile_once)
//! against a modelled reconcile world ([`reconcile`]) and the now-live
//! adversarial [`ScriptedResponse`] queue. Oracle #9 (the None-arm mis-reap
//! stays fixed) joins the invariant suite. See [`reconcile`].
//!
//! # Deferred to P4+
//!
//! The remaining lifecycle-flow extractions (Flow A/B/D/E), scheduler-driven
//! crash-point INJECTION (the [`CrashPoint`] boundaries are named +
//! coverage-tested here, but cutting the process at one is P4), and the #224
//! insert-after-sweep gate (it lives in the `abandon_nbd_data_planes_for_shutdown`
//! SIGTERM path, which is not extracted until P4's Flow A).

pub mod coord_stub;
pub mod effects;
pub mod invariants;
pub mod reconcile;
pub mod scheduler;
pub mod simfs;
pub mod world;

pub use coord_stub::{RecordedPublish, ScriptedResponse, SimCoordClient};
pub use effects::{sim_effects, SeamEvent, SeamLog, SimDeviceSync, SimNbd};
pub use invariants::Violation;
pub use reconcile::{DestroyRecord, SimReconcileBackend};
pub use scheduler::{Profile, Sim, SimReport, Step, NUM_SANDBOXES};
pub use simfs::{CrashPoint, SimFs};
pub use world::{AckedWriteLedger, LedgerEntry, SandboxSlot, SimHost, CHUNK_SIZE, NUM_CHUNKS};
