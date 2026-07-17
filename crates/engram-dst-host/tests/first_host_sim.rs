//! The first host-internal sims (ADR 0098 Phase 2, P2): calm+chaos smoke
//! over a seed range, plus the two determinism self-checks.
//!
//! Sizing: each step drives the REAL shutdown-spool machinery, whose per-op
//! `fsync`s make the sim I/O-wait bound (not CPU bound) — so these in-CI
//! tests prove the property (acked-write durability + replay determinism)
//! over a modest seed×step budget, and the broad net is the release swarm
//! (`just sim-host-swarm 0..50 400`, the P9 CI lane). Calm and chaos are
//! split into separate `#[test]` fns so nextest overlaps their fsync waits
//! across cores (AGENTS.md: size a test to the property, minimize CI time).

use engram_dst_host::{Profile, Sim};

const SMOKE_SEEDS: u64 = 8;
const SMOKE_STEPS: u64 = 200;

async fn run_smoke(seed: u64, profile: Profile, steps: u64) {
    let mut sim = Sim::new(seed, profile).await;
    let report = sim.run(steps).await.unwrap_or_else(|e| {
        let tail: Vec<_> = sim
            .report()
            .trace
            .iter()
            .rev()
            .take(24)
            .rev()
            .cloned()
            .collect();
        panic!("seed {seed} ({profile:?}) failed: {e}\n  trace tail: {tail:#?}")
    });
    assert_eq!(report.steps_run, steps);
}

/// Calm profile: pure guest workload + flush/spool machinery, no crashes.
/// Every seed recovers every acked write across the quiesce crash cycle.
#[tokio::test(start_paused = true)]
async fn acked_write_durability_holds_calm() {
    for seed in 0..SMOKE_SEEDS {
        run_smoke(seed, Profile::Calm, SMOKE_STEPS).await;
    }
}

/// Chaos profile: process crash/restart interleavings on top of the
/// workload. The shipped spool-then-die + rebuild+adopt machinery must
/// recover every acked write at every crash→restart.
#[tokio::test(start_paused = true)]
async fn acked_write_durability_holds_chaos() {
    for seed in 0..SMOKE_SEEDS {
        run_smoke(seed, Profile::Chaos, SMOKE_STEPS).await;
    }
}

/// Determinism rule: the same seed run twice produces byte-identical traces.
/// This is the replay-twice-and-diff self-check (ADR 0098 risk #2).
#[tokio::test(start_paused = true)]
async fn same_seed_replays_identically() {
    for seed in [0u64, 3, 42] {
        let mut a = Sim::new(seed, Profile::Chaos).await;
        let ra = a.run(SMOKE_STEPS).await.expect("run a");
        let mut b = Sim::new(seed, Profile::Chaos).await;
        let rb = b.run(SMOKE_STEPS).await.expect("run b");
        assert_eq!(
            ra.trace, rb.trace,
            "seed {seed}: traces diverged across runs"
        );
        assert_eq!(
            ra.writes_acked, rb.writes_acked,
            "seed {seed}: write count diverged"
        );
    }
}

/// Distinct seeds explore distinct interleavings — the pick stream actually
/// depends on the seed.
#[tokio::test(start_paused = true)]
async fn distinct_seeds_diverge() {
    let mut a = Sim::new(1, Profile::Chaos).await;
    let ra = a.run(SMOKE_STEPS).await.expect("run a");
    let mut b = Sim::new(2, Profile::Chaos).await;
    let rb = b.run(SMOKE_STEPS).await.expect("run b");
    assert_ne!(ra.trace, rb.trace, "distinct seeds must diverge");
}
