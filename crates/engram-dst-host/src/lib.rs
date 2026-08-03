//! The host-internal deterministic simulator (ADR 0098 and ADR 0110).
//!
//! This crate drives portable host-agent code over seeded world inputs. Each
//! run uses a current-thread Tokio runtime with paused time. A seeded ChaCha8
//! stream selects steps. Ordered collections make all decisions stable.
//!
//! The disk model uses one stable dirty file for each sandbox. A guest write
//! reaches this file before the backend returns. Linux process-death flows drop
//! the backend and reopen the file in recovery mode. The sidecar selects a
//! store-ahead manifest when the coordinator ref is stale. Oracle #1 requires
//! every chunk to return its exact latest acked tag after recovery.
//!
//! The flows the scheduler drives, and where each came from:
//!
//! * **Flow C — reconcile (P3):** [`ReconcileTick`](Step::ReconcileTick) plus
//!   the [`DropLocalBinding`](Step::DropLocalBinding) /
//!   [`RevokeOwnership`](Step::RevokeOwnership) perturbations run the REAL
//!   `reconcile_once` against the modeled reconcile world. Oracle #9 pins the
//!   None-arm mis-reap fix. See [`reconcile`].
//! * **Flow D — eviction finalize (P5):**
//!   [`SnapshotBegin`](Step::SnapshotBegin) /
//!   [`FinalizeTick`](Step::FinalizeTick) /
//!   [`FinalizeCrashAt`](Step::FinalizeCrashAt) drive the REAL finalize legs.
//!   [`CrashFs`] cuts `durable_record` and finalize file operations at seeded
//!   op boundaries; the crash schedule derives from the production op
//!   sequence (`tests/crashpoint_coverage.rs` pins the derivation). Oracles
//!   #6 (stage monotone) and #8 (convergence at quiescence) guard it.
//! * **Flow B — the slot/reattach device lifecycle (P7):**
//!   [`SlotClaim`](Step::SlotClaim), [`Park`](Step::Park),
//!   [`Unpause`](Step::Unpause), [`RegisterRehydrate`](Step::RegisterRehydrate),
//!   [`StaleSweepTick`](Step::StaleSweepTick). Oracles #3 (slot accounting)
//!   and #5 (single-device ownership, no severed live holder) guard it.
//! * **Flow F — the flush-pipeline seam (P6):**
//!   [`FlushHandoffRace`](Step::FlushHandoffRace) /
//!   [`FlushFenceAbort`](Step::FlushFenceAbort) /
//!   [`FlushPreRebaseCrash`](Step::FlushPreRebaseCrash) park a REAL `flush()`
//!   at the three `FlushSeamPoint`s and interleave reads, writes, fences,
//!   and process death against it.
//! * **Flow E — migration (P8):** the `Migration*` steps run the REAL
//!   `MigrationRegistry` over injected time. Oracle #7 pins the #216
//!   decision table; oracle #2 pins plane accounting.
//!
//! **Hard boundary:** this crate does not depend on `engram-coordinator`.
//! The coordinator and host simulators sit in disjoint cargo-dep closures so
//! the CI detector runs each lane independently.
//!
//! [`Step::ReconcileTick`]: scheduler::Step::ReconcileTick
//! [`Step::DropLocalBinding`]: scheduler::Step::DropLocalBinding
//! [`Step::RevokeOwnership`]: scheduler::Step::RevokeOwnership
//! [`Step::SnapshotBegin`]: scheduler::Step::SnapshotBegin
//! [`Step::FinalizeTick`]: scheduler::Step::FinalizeTick
//! [`Step::FinalizeCrashAt`]: scheduler::Step::FinalizeCrashAt
//! [`Step::SlotClaim`]: scheduler::Step::SlotClaim
//! [`Step::Park`]: scheduler::Step::Park
//! [`Step::Unpause`]: scheduler::Step::Unpause
//! [`Step::RegisterRehydrate`]: scheduler::Step::RegisterRehydrate
//! [`Step::StaleSweepTick`]: scheduler::Step::StaleSweepTick
//! [`Step::FlushHandoffRace`]: scheduler::Step::FlushHandoffRace
//! [`Step::FlushFenceAbort`]: scheduler::Step::FlushFenceAbort
//! [`Step::FlushPreRebaseCrash`]: scheduler::Step::FlushPreRebaseCrash

pub mod coord_stub;
pub mod device_plane;
pub mod effects;
pub mod fs_crash;
pub mod invariants;
pub mod reconcile;
pub mod scheduler;
pub mod simfs;
pub mod world;

pub use coord_stub::{RecordedPublish, ScriptedResponse, SimCoordClient};
pub use device_plane::{DevicePlane, DeviceSlot, ServeOutcome};
pub use effects::{sim_effects, SeamEvent, SeamLog, SimDeviceSync, SimNbd};
pub use fs_crash::{CrashFs, FsOp, ReadCorruption, ReadFault};
pub use invariants::Violation;
pub use reconcile::{DestroyRecord, SimReconcileBackend};
pub use scheduler::{Profile, Sim, SimReport, Step, NUM_SANDBOXES};
pub use simfs::SimFs;
pub use world::{
    decode_tag, synth_chunk, AckedWriteLedger, CaptureOutcome, LedgerEntry, ResumeOutcome,
    SandboxSlot, SimEvictionSandbox, SimHost, CHUNK_SIZE, NUM_CHUNKS, SIM_FINALIZE_MAX_ATTEMPTS,
};
