//! Portable host-agent core (ADR 0098 Phase 2, the P-series).
//!
//! This crate holds the host-agent's side-effect seams and the
//! prod-portable impls of them. It depends only on `engram-core` (plus the
//! ambient async/serde/time crates) — deliberately NOT on
//! `engram-host-agent`, reqwest, or anything Linux-gated — which is what
//! makes the extracted lifecycle logic **runnable on macOS** and drivable
//! by the host-internal simulator (`engram-dst-host`).
//!
//! # Why a bundle struct, not a mega-trait
//!
//! [`HostEffects`] is a bundle STRUCT (`clock` + `entropy` + four focused
//! traits), mirroring the coordinator's `Services` — not one god
//! `HostEffects` trait. ADR 0098 §Phase 2 makes this explicit: a single
//! mega-trait would recreate exactly the mock-zoo that D4 retired, and it
//! would force every flow to depend on the union of all effects. Instead
//! each seam is its own trait at an orchestration boundary:
//!
//! - [`coord::CoordControlPlane`] — the three decision-feeding coordinator
//!   calls (`publish_live_manifest`, `sandbox_ownership`, `sandbox_owner`);
//!   the concrete `HttpCoordClient` in `engram-host-agent` implements it,
//!   and the simulator supplies an adversarial scripted stub.
//! - [`fs::HostFs`] — granular durable-fs primitives (write / sync_file /
//!   rename / sync_dir / read / read_dir / remove_file) so a crash injector
//!   can fail between each.
//! - [`device::DeviceSync`] — the `/dev/nbdN` host-page-cache sync
//!   (abandon-in-place stays a `NbdSandboxState` method; see `device`).
//! - [`nbd::NbdKernel`] — the connect/reconfigure/disconnect/
//!   backend-identifier kernel ops.
//!
//! The data plane (the NBD serve loop, `read_chunk`/`write_chunk`, the
//! migrate_peer page server, the flush lock ladder) is deliberately NOT
//! behind a seam — wrapping the hot path would add per-op cost for logic
//! the simulator never drives.

pub mod checkpoint;
pub mod coord;
pub mod device;
pub mod effects;
pub mod finalize;
pub mod fs;
pub mod nbd;
pub mod reattach;
pub mod shutdown;
pub mod survivor;

pub use checkpoint::checkpoint_tail_admits_publish;
pub use coord::{
    CoordControlPlane, CoordError, LiveManifestPublishOutcome, LiveManifestPublishRequest,
    LiveManifestPublishResponse,
};
pub use device::DeviceSync;
pub use effects::HostEffects;
pub use finalize::{plan_finalize_retry, FinalizeRetry, FinalizeStage};
pub use fs::{HostFs, TokioFs};
pub use nbd::{NbdConnectRequest, NbdKernel, NbdReconfigureRequest};
pub use reattach::{
    first_seeded_probe, is_local_survivor_candidate, plan_reattach, probe_matches,
    resume_data_plane_served, sweep_verdict, DeviceHolder, PidLiveness, ReattachPlan, ReattachStep,
    SweepAction,
};
pub use survivor::{
    plan_capture_disk_drain, plan_resume_attach, CaptureDrainPlan, ResumeAttachPlan,
};

pub use shutdown::{
    admits_new_plane, classify_survivor, flush_budget, is_straggler, plan_shutdown, FlushProbe,
    ShutdownPlan, ShutdownStage, SurvivorAction, DEFAULT_FLUSH_BUDGET_SECS,
};
