//! The first end-to-end simulations (ADR 0098 D5).
//!
//! Fixed seeds — a CI failure is always locally reproducible with
//! `Sim::new(<seed>, <profile>).run(<steps>)`. Every seed that finds a
//! real bug gets pinned in tests/regression_seeds.rs with a comment
//! naming the fix.

use engram_dst::{Profile, Sim};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("current-thread runtime")
}

/// Calm profile: workload + drivers, no faults. The liveness baseline —
/// every session must reach a stable state at quiescence.
#[test]
fn calm_seeds_converge() {
    let rt = rt();
    rt.block_on(async {
        tokio::time::pause();
        for seed in 0..24u64 {
            let mut sim = Sim::new(seed, Profile::Calm);
            match sim.run(400).await {
                Ok(report) => {
                    assert_eq!(report.steps_run, 400);
                }
                Err(msg) => panic!(
                    "calm seed {seed} violated an invariant after {} steps: {msg}\nlast trace:\n{}",
                    sim.report().steps_run,
                    sim.report()
                        .trace
                        .iter()
                        .rev()
                        .take(25)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            }
        }
    });
}

/// Chaos profile: host crashes/restarts, replica crashes/restarts, PG
/// outage windows — the system must still converge once faults heal.
#[test]
fn chaos_seeds_converge() {
    let rt = rt();
    rt.block_on(async {
        tokio::time::pause();
        for seed in 0..24u64 {
            let mut sim = Sim::new(seed, Profile::Chaos);
            match sim.run(600).await {
                Ok(_) => {}
                Err(msg) => panic!(
                    "chaos seed {seed} violated an invariant after {} steps: {msg}\nlast trace:\n{}",
                    sim.report().steps_run,
                    sim.report().trace.iter().rev().take(25).cloned().collect::<Vec<_>>().join("\n"),
                ),
            }
        }
    });
}

/// THE determinism contract: the same seed produces byte-identical
/// traces on two independent runs. This is the replay-twice-and-diff
/// self-check from the ADR — the guard against detached-spawn /
/// HashMap-iteration / unbiased-select leaks making seeds
/// non-replayable (the most demoralizing DST failure mode).
#[test]
fn same_seed_replays_identically() {
    let run = |seed: u64| {
        let rt = rt();
        rt.block_on(async {
            tokio::time::pause();
            let mut sim = Sim::new(seed, Profile::Chaos);
            match sim.run(500).await {
                Ok(r) => r.trace,
                Err(_) => std::mem::take(&mut sim.report_mut().trace),
            }
        })
    };
    for seed in [3u64, 7, 42] {
        let a = run(seed);
        let b = run(seed);
        assert_eq!(a.len(), b.len(), "seed {seed}: trace lengths diverged");
        for (i, (x, y)) in a.iter().zip(&b).enumerate() {
            assert_eq!(x, y, "seed {seed}: traces diverged at step {i}");
        }
    }
}

/// Distinct seeds explore distinct interleavings (a degenerate PRNG or
/// a swallowed seed would silently collapse the swarm's coverage).
#[test]
fn distinct_seeds_diverge() {
    let run = |seed: u64| {
        let rt = rt();
        rt.block_on(async {
            tokio::time::pause();
            let mut sim = Sim::new(seed, Profile::Chaos);
            match sim.run(200).await {
                Ok(r) => r.trace,
                Err(_) => std::mem::take(&mut sim.report_mut().trace),
            }
        })
    };
    assert_ne!(run(1), run(2), "different seeds must explore differently");
}

/// The DriverKind list tracks the coordinator's driven surface: the sim
/// must step every driver family the first-sim scope claims. (Grows in
/// D6 to assert against the full run_once inventory.)
#[test]
fn driver_coverage_is_declared() {
    // Compile-time proof the four entry points remain callable from
    // outside the crate (visibility regressions break this test's
    // BUILD, which is the point).
    let _ = engram_coordinator::queue_scanner::run_once;
    let _ = engram_coordinator::dead_host::run_once;
    let _ = engram_coordinator::session_ops::drive_session;
    let _ = engram_coordinator::session_ops::drive_claimed;
    let _ = engram_coordinator::session_ops::enqueue;
    let _ = <engram_sim::SimMetadataStore as engram_core::traits::MetadataStore>::apply_missing_sandbox_strikes;
}
