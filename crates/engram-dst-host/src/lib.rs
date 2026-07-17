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
//! # P4 (Flow A — the SIGTERM ladder)
//!
//! P4 drives the REAL extracted shutdown ladder
//! ([`engram_host_core::shutdown`]): the [`Sigterm`](Step::Sigterm) step runs
//! `plan_shutdown`/`classify_survivor` over the sim host (a seeded budget
//! below the modeled flush cost overruns → stragglers ride the spool, #225),
//! and [`CrashAt`](Step::CrashAt) seeds crash-point INJECTION at the eight
//! [`CrashPoint`] boundaries — the spool boundaries land the real recovery
//! under the acked-write oracle, the persist boundaries exercise
//! `durable_record` tolerance (folding into the ledger in P5's Flow D). The
//! world-model `rebuild` gains the 85e0298a store-ahead rule (attach from the
//! spool's ahead ref, never coord's stale one). The #224 insert-after-sweep
//! gate rides as the extracted ordering contract
//! ([`admits_new_plane`](engram_host_core::admits_new_plane)); the literal
//! `abandon_nbd_data_planes_for_shutdown` DashMap race stays FC-lane residue.
//! See [`crash_state`] and [`world`].
//!
//! # P4.5 (oracle honesty — the durability pipeline, not omniscient recovery)
//!
//! The acked-write oracle ([`invariants`]) is sharpened from "every acked write
//! recovers" to the honest range `[published_floor, latest_ack]`. The ledger
//! tracks the published-tier FLOOR per chunk (raised only by a real
//! [`FlushTick`](Step::FlushTick) / the SIGTERM ladder's publish leg, observed
//! from the durable manifest): a PUBLISHED write must never roll back, but a
//! write lost to abrupt death before it is published is an accepted, bounded
//! loss. A shutdown-spool capture is a TRANSIENT handoff (the successor adopts
//! it back into the volatile tier), so it does NOT raise the floor; spool
//! recovery is asserted by the regression seeds that crash with a standing
//! spool. The new [`AbruptCrash`](Step::AbruptCrash) step drops RAM with NO
//! spool to exercise the post-ack/pre-publish window every prior crash
//! primitive (all spool first) never reached — which is what surfaced that a
//! sticky spool floor over-claims. See the regression seed
//! `post_ack_pre_handoff_crash_is_honest_loss`.
//!
//! # Deferred to P5+
//!
//! The remaining lifecycle-flow extractions (Flow B/D/E) and their oracles.

pub mod coord_stub;
pub mod crash_state;
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
pub use world::{
    decode_tag, AckedWriteLedger, LedgerEntry, SandboxSlot, SimHost, CHUNK_SIZE, NUM_CHUNKS,
};
