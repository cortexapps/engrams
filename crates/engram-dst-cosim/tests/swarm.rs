//! Rung 2 (ADR 0098 R-CoSim, #784): the boundary-swarm smoke + the replay-twice
//! determinism self-check, run in the `test-cosim` nextest lane.
//!
//! The full seed windows run in the `sim-cosim` binary (ci.yml `test-cosim`
//! swarm step + the nightly `cosim-swarm` job). These in-lane tests keep the
//! swarm's DETERMINISM (audit items 6–8) and convergence honest on every PR
//! without the release-build cost of a full window.

use engram_dst_cosim::{run_seed, Profile};

/// Replay-twice: two runs of the same seed on independent per-seed runtimes
/// must produce byte-identical pick traces AND the same verdict. This is the
/// determinism guard — a fs-I/O clock auto-advance that tipped a decision
/// boundary would diverge here (the #799 class the coarse-time discipline
/// prevents).
#[test]
fn replay_twice_is_deterministic() {
    for seed in [1u64, 7, 19] {
        let a = run_seed(seed, Profile::Chaos, 400, usize::MAX);
        let b = run_seed(seed, Profile::Chaos, 400, usize::MAX);
        assert_eq!(
            a.result.is_ok(),
            b.result.is_ok(),
            "seed {seed}: two runs disagreed on the verdict — a determinism leak"
        );
        assert_eq!(
            a.trace, b.trace,
            "seed {seed}: two runs produced different pick traces — a determinism leak \
             (the #799 fs-I/O clock-drift class)"
        );
    }
}

/// A small calm + chaos window converges with every standing oracle holding —
/// the in-lane smoke (the wide windows run in `sim-cosim`).
#[test]
fn small_window_converges() {
    for seed in 0u64..6 {
        for profile in [Profile::Calm, Profile::Chaos] {
            let out = run_seed(seed, profile, 300, 40);
            if let Err(msg) = out.result {
                panic!(
                    "seed {seed} ({profile:?}) failed: {msg}\n  trace tail:\n{}",
                    out.trace
                        .iter()
                        .map(|l| format!("    {l}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
        }
    }
}
