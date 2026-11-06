//! Portable host-agent core (ADR 0098 Phase 2, the P-series).
//!
//! This crate holds the host-agent's side-effect seams and the
//! prod-portable impls of them. It depends on `engram-core` and
//! `engram-chunk-store` (plus the ambient async/serde/time crates) —
//! deliberately NOT on `engram-host-agent`, reqwest, or anything
//! Linux-gated — which is what makes the extracted lifecycle logic
//! **runnable on macOS** and drivable by the boundary simulator
//! (`engram-dst-cosim`).
//!
//! # Seams
//!
//! Each side effect a lifecycle flow makes sits behind its own focused
//! trait at an orchestration boundary, never one god trait:
//!
//! - [`coord::CoordControlPlane`] — the decision-feeding coordinator calls
//!   (`sandbox_ownership`, `sandbox_owner`); the concrete `HttpCoordClient`
//!   in `engram-host-agent` implements it, and the simulator answers it
//!   from the real coordinator handler cores.
//! - [`fs::HostFs`] — granular durable-fs primitives (write / sync_file /
//!   rename / sync_dir / read / read_dir / remove_file) so a crash injector
//!   can fail between each.
//! - [`disk::SessionDisk`] — volumes the kernel owns (create, O(1)
//!   snapshot, delete, dirty-chunk diff, cloned drives); [`disk_flows`] moves chunks
//!   between a volume and the chunk store; [`image_volumes`] keeps the
//!   enabled images staged and [`session_volumes`] owns every session's
//!   volumes.

pub mod checkpoint;
pub mod coord;
pub mod disk;
pub mod disk_flows;
pub mod finalize;
pub mod fs;
pub mod image_volumes;
pub mod session_volumes;
pub mod shutdown;
#[cfg(test)]
mod test_support;

pub use checkpoint::checkpoint_tail_admits_publish;
pub use coord::{CoordControlPlane, CoordError};
pub use disk::{
    is_zero_chunk, Capacity, ChunkIndex, CloneState, FileDisk, Hydration, SessionDisk, Volume,
    VolumeName,
};
pub use disk_flows::{checkpoint, checkpoint_split, stage};
pub use finalize::{plan_finalize_retry, FinalizeRetry, FinalizeStage};
pub use fs::{HostFs, TokioFs};
pub use session_volumes::SessionVolumes;
pub use shutdown::{capture_drain_budget, HandoffMarker};
