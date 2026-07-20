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
pub mod model;
pub mod scheduler;
pub mod workload;
pub mod world;

pub use scheduler::{DriverKind, Profile, Sim, SimReport, Step};
pub use world::SimWorld;

/// The outcome of running ONE seed to a verdict (converged, or an
/// invariant/liveness violation) on its own hermetic runtime.
pub struct SeedOutcome {
    pub seed: u64,
    /// `Ok(report)` = converged; `Err(msg)` = an invariant/quiescence
    /// violation, message shaped `step N: <slug> — <detail>` /
    /// `quiescence: <slug> — <detail>`.
    pub result: Result<SimReport, String>,
    /// The last `trace_tail` trace lines, for the failure artifact.
    /// Populated only on `Err` (a converged run replaces its trace out).
    pub trace: Vec<String>,
}

/// Run ONE seed on its OWN current-thread paused-clock runtime, then DROP
/// that runtime before returning — the seed-independence primitive the
/// whole swarm rests on.
///
/// Why a runtime PER seed (not one shared across a `--seeds` range): the
/// workload drives the REAL coordinator handlers, and those legitimately
/// spawn DETACHED tokio tasks — the op executor's completion re-drive
/// (`drive_claimed`/`drive_session` off `session_ops::enqueue`), the 15 s
/// within-step `op-heartbeat` INTERVAL, the outbox `Deliver` op the
/// prompt path fires. Some of those park on **tokio timers**, and
/// `SimClock` *is* tokio's paused clock (`clock.rs`: `advance` ==
/// `tokio::time::advance`), so a timer-parked detached task is a live
/// participant in the sim's time model that the step-loop's
/// `drain_detached` (bare `yield_now`s, which never move virtual time)
/// cannot reap. On a SHARED runtime those tasks LEAK across the seed
/// boundary and pile up; eventually a later seed's runtime parks the OS
/// thread waiting on the accumulated cross-seed timer/waker state instead
/// of auto-advancing virtual time — a **paused-clock deadlock** that
/// manifests as the multi-hour CI hang (reproduced on Linux under load at
/// varying seeds, and intermittently on macOS under repeated runs; NEVER
/// when a seed runs in isolation). Dropping the runtime at the seed
/// boundary reaps every detached task, making each seed hermetic — which
/// is exactly what a single `--seed N` replay already does.
pub fn run_seed(
    seed: u64,
    profile: Profile,
    steps: u64,
    faithful: bool,
    trace_tail: usize,
) -> SeedOutcome {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("current-thread runtime");
    let outcome = rt.block_on(async move {
        tokio::time::pause();
        let mut sim = Sim::new(seed, profile);
        if faithful {
            sim = sim.with_faithful_hosts();
        }
        let result = sim.run(steps).await;
        let trace = sim
            .report()
            .trace
            .iter()
            .rev()
            .take(trace_tail)
            .rev()
            .cloned()
            .collect::<Vec<_>>();
        SeedOutcome {
            seed,
            result,
            trace,
        }
    });
    // `rt` drops here: every detached task this seed spawned is aborted,
    // so nothing crosses into the next seed.
    drop(rt);
    outcome
}
