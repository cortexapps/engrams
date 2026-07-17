//! Pinned regression seeds (ADR 0098 D7).
//!
//! Every simulator-found bug gets its seed pinned HERE with a comment
//! naming the finding and its fix — the sim-swarm's permanent memory.
//! Random exploration lives in the CI swarm (`just sim-swarm`); this
//! file replays known-bad interleavings forever.

use engram_dst::{Profile, Sim};

fn run(seed: u64, profile: Profile, steps: u64) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(seed, profile);
        if let Err(msg) = sim.run(steps).await {
            panic!(
                "pinned seed {seed} regressed: {msg}\ntrace tail:\n{}",
                sim.report()
                    .trace
                    .iter()
                    .rev()
                    .take(25)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    });
}

/// Issue #722: chaos seed 0 first exposed the placement over-reservation
/// hole (the 10-minute crash-orphan exclusion vs the ADR 0079 pending
/// revival backstop) at step 547 under the UNCONDITIONAL accounting
/// oracle. The shipped oracle is scoped to placement's self-consistent
/// arithmetic until #722's fix lands; this pin keeps the interleaving
/// alive so tightening the oracle back re-tests the exact scenario.
#[test]
fn issue_722_placement_over_reservation_interleaving() {
    run(0, Profile::Chaos, 600);
}
