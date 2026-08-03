//! The first host-internal sims (ADR 0098 Phase 2, P2): calm+chaos smoke
//! over a seed range, plus the two determinism self-checks.
//!
//! These tests prove acked-write durability and replay determinism over a
//! small seed and step budget. The release swarm provides the broad run.

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

/// Run the guest and flush baseline without generated process deaths.
#[tokio::test(start_paused = true)]
async fn acked_write_durability_holds_calm() {
    for seed in 0..SMOKE_SEEDS {
        run_smoke(seed, Profile::Calm, SMOKE_STEPS).await;
    }
}

/// Add Linux process death and dirty-file recovery to the workload.
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
