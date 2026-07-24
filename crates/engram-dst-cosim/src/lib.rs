//! The coordinator↔host boundary simulator (ADR 0098 R-CoSim, rung 1).
//!
//! `engram-dst` drives the real coordinator drivers over `engram-sim`;
//! `engram-dst-host` drives the real host-agent flows over its own world.
//! By construction those two sims are **disjoint** — separate cargo-dep
//! closures, separate clocks — so the coordinator↔host boundary (where
//! #570, #739, #602, #216, and 85e0298a all lived) is explored by NEITHER.
//! This crate is that boundary: rung 1 wires BOTH real sides against **one
//! shared `SimClock` + `SimMetadataStore`** and fakes only the transport
//! between them.
//!
//! # What is real vs faked
//!
//! **Coordinator side (real):** a full `AppState`/`Services` replica over the
//! shared `SimMetadataStore` + `SimClock`/`SimEntropy` — the exact substrate
//! `engram-dst` builds. Real drivers (`idle_detector::run_once`,
//! `idle_evictor::scanner_run_once`, `session_ops::drive_session`,
//! `queue_scanner::run_once`) and real op verbs (create_boot, evict, resume)
//! run unmodified. The three coordinator answers the host reads
//! (`live_manifest_publish`, `sandbox_ownership`, `sandbox_owner`) run their
//! REAL handler cores (extracted to `pub` fns in
//! `engram_coordinator::api::host_http` — the run_once pattern applied to
//! handlers).
//!
//! **Host side (real):** a [`CosimHost`](host::CosimHost) running the REAL
//! host-agent lifecycle flows — the `EvictionFinalizer` capture +
//! `run_eviction_finalize_attempt` finalize legs, a real `ChunkedDiskBackend`
//! over a real `ChunkStore`, and the REAL
//! [`reconcile_once`](engram_host_agent::teardown_reconcile::reconcile_once)
//! teardown-reconcile tick. ADR 0103 also composes the real Firecracker
//! host-side exec driver with real agentd connection handlers over a
//! per-sandbox durable journal; the duplex/vsock severance is the only fake.
//!
//! **Faked (only the transport):** the [`bridge`] — [`CosimHostClient`]
//! (coordinator→host: the `HostClient` verbs drive the host's real ops) and
//! [`CosimCoordControlPlane`] (host→coordinator: the `CoordControlPlane`
//! calls the real coordinator cores). No sockets; a direct call is the wire.
//!
//! # The standing oracle
//!
//! [`Cosim::assert_idle_snapshot_durable`](scheduler::Cosim::assert_idle_snapshot_durable):
//! *the coordinator reaching `Idle` via eviction implies the snapshot its
//! resume path selects is durable and at-or-above the eviction cursor.* This
//! is exactly what issue #570 violates — see
//! `tests/unbind_vs_teardown_reconcile.rs`.
//!
//! # Divergence from the ADR plan (documented, per "follow unless the code
//! proves it wrong")
//!
//! The plan said "reuse `engram-dst-host`'s `SimHost`." `SimHost::new` seeds
//! a fixed slot id-space and assumes it owns the session/sandbox ids; at this
//! boundary the coordinator mints those ids, so its top-level slot model is
//! wrong here. We instead reuse the *real extracted flows* `SimHost` is built
//! on (the finalizer machinery, `ChunkedDiskBackend`, `reconcile_once`,
//! `SimFs`) keyed by the coordinator's id-space. See [`host`].

pub mod bridge;
pub mod host;
pub mod scheduler;
pub mod swarm;
pub mod world;

pub use bridge::{CosimCoordControlPlane, CosimHostClient};
pub use host::{CosimHost, CosimReconcileBackend, FinalizeTickOutcome, HostView, SharedHost};
pub use scheduler::Cosim;
pub use swarm::{run_seed, CosimSwarm, Profile, SeedOutcome, Step, SwarmReport};
pub use world::{CosimWorld, COSIM_IMAGE};
