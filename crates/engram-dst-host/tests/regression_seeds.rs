//! Pinned reconcile scenarios + regression seeds (ADR 0098 Phase 2, P3 —
//! Flow C).
//!
//! Mirrors `engram-dst`'s `regression_seeds.rs`: deterministic, replayable
//! interleavings that pin a property forever. Random exploration lives in the
//! swarm (`just sim-host-swarm` / `sim-host --seeds …`); this file replays the
//! headline Flow C cases — Oracle #9 (the 2026-07-11 mis-reap stays fixed),
//! the coordinator-flip adversarial cases, the strike-debounce, the
//! destroy-retry, and the slot-allocator stale-binding TOCTOU — plus a few
//! pinned chaos seeds that keep the reconcile interleavings alive.

use std::collections::HashMap;

use engram_dst_host::{invariants, Profile, ScriptedResponse, Sim, SimHost};

/// Drive one deterministic scenario host with `num` sandboxes.
async fn scenario_host(seed: u64, num: usize) -> SimHost {
    SimHost::new(seed, num).await
}

// ─────────────────────────── Oracle #9 ────────────────────────────
//
// The headline pin: a sandbox the coordinator owns but with NO local binding
// must be REPAIRED and NEVER reaped — the 2026-07-11 mis-reap (ADR 0090). We
// drop the local binding, then run MORE than ORPHAN_STRIKES reconcile ticks;
// the sandbox stays live, its binding is repaired, and nothing is destroyed.

#[tokio::test(start_paused = true)]
async fn none_arm_coord_owned_unbound_sandbox_is_repaired_never_reaped() {
    let host = scenario_host(0, 3).await;
    let sandbox_id = host.sandboxes[0].sandbox_id;
    let session_id = host.sandboxes[0].session_id;

    // ADR 0090 survivor: the local binding is gone (a fresh generation whose
    // NBD rehydrate bailed before the insert), but the coordinator still owns
    // it.
    host.drop_local_binding(0);
    assert!(host.reconcile.binding(sandbox_id).is_none());

    let mut strikes = HashMap::new();
    for tick in 0..5 {
        host.reconcile_tick(&mut strikes).await.unwrap();
        invariants::check(&host)
            .await
            .unwrap_or_else(|v| panic!("tick {tick}: {} — {}", v.invariant, v.detail));
    }

    assert!(
        host.reconcile.is_live(sandbox_id),
        "a coord-owned sandbox must NEVER be reaped, however long it stays unbound",
    );
    assert_eq!(
        host.reconcile.binding(sandbox_id),
        Some(session_id),
        "the missing local binding must be repaired from the coordinator's answer",
    );
    assert!(
        host.reconcile.destroyed().is_empty(),
        "no destroy may target a coord-owned sandbox",
    );
    // The repair was recorded exactly once (the first tick), then steady.
    assert_eq!(
        host.reconcile
            .binding_repairs()
            .iter()
            .filter(|(id, _)| *id == sandbox_id)
            .count(),
        1,
        "the binding is repaired once, then the sandbox is bound and quiet",
    );
}

/// The unbound-AND-unreachable edge of the same fix: coord owns it but is
/// unreachable, so the answer is `Err` → assume owned → never reaped (and the
/// binding is NOT repaired from a non-answer).
#[tokio::test(start_paused = true)]
async fn none_arm_unbound_but_coord_unreachable_is_never_reaped() {
    let host = scenario_host(0, 1).await;
    let sandbox_id = host.sandboxes[0].sandbox_id;
    host.drop_local_binding(0);

    // Two consecutive unreachable ticks: the unbound arm sees `Err` → Owned.
    host.coord
        .script(ScriptedResponse::Unreachable("sim: coord down"));
    host.coord
        .script(ScriptedResponse::Unreachable("sim: coord down"));

    let mut strikes = HashMap::new();
    for _ in 0..2 {
        host.reconcile_tick(&mut strikes).await.unwrap();
        invariants::check(&host).await.unwrap();
    }
    assert!(host.reconcile.is_live(sandbox_id));
    assert!(host.reconcile.destroyed().is_empty());
    assert!(
        host.reconcile.binding(sandbox_id).is_none(),
        "a coord non-answer must not fabricate a binding",
    );
}

// ─────────────────── coordinator-flip adversarial cases ───────────────────
//
// The coord stub's scripted queue, fired for real (wired-but-benign in P2).
// Single-sandbox hosts so the global script queue maps 1:1 onto the tick's
// one ownership call.

/// Ownership answered `false` twice ⇒ destroyed on the 2nd strike (correct):
/// ownership genuinely departed (revoked in the honest model too), so the reap
/// is right and Oracle #9 holds.
#[tokio::test(start_paused = true)]
async fn coord_flip_ownership_false_twice_reaps_on_second_strike() {
    let host = scenario_host(0, 1).await;
    let sandbox_id = host.sandboxes[0].sandbox_id;

    // Departed ownership: revoke the honest model so the reap is CORRECT, and
    // fire the adversarial script so the answer is deterministically false.
    host.revoke_ownership(0);
    host.coord.script(ScriptedResponse::Ownership(false));
    host.coord.script(ScriptedResponse::Ownership(false));

    let mut strikes = HashMap::new();
    host.reconcile_tick(&mut strikes).await.unwrap(); // strike 1
    assert!(
        host.reconcile.is_live(sandbox_id),
        "one orphan strike must NOT reap (the create→bind debounce)",
    );
    invariants::check(&host).await.unwrap();

    host.reconcile_tick(&mut strikes).await.unwrap(); // strike 2 → destroy
    assert!(
        !host.reconcile.is_live(sandbox_id),
        "the second consecutive orphan strike reaps a genuinely-departed sandbox",
    );
    invariants::check(&host).await.unwrap();
    assert_eq!(host.reconcile.destroyed().len(), 1);
}

/// Coordinator unreachable twice ⇒ never destroyed: a still-owned sandbox
/// whose coordinator is briefly unreachable stays live (the `Err ⇒ assume
/// owned` posture — never reap on a control-plane blip).
#[tokio::test(start_paused = true)]
async fn coord_unreachable_twice_never_reaps() {
    let host = scenario_host(0, 1).await;
    let sandbox_id = host.sandboxes[0].sandbox_id;

    // Keep honest ownership (the sandbox IS owned); the coordinator just can't
    // be reached for two ticks.
    host.coord
        .script(ScriptedResponse::Unreachable("sim: coord unreachable"));
    host.coord
        .script(ScriptedResponse::Unreachable("sim: coord unreachable"));

    let mut strikes = HashMap::new();
    for _ in 0..3 {
        host.reconcile_tick(&mut strikes).await.unwrap();
        invariants::check(&host).await.unwrap();
    }
    assert!(
        host.reconcile.is_live(sandbox_id),
        "an unreachable coordinator must never cause a reap",
    );
    assert!(host.reconcile.destroyed().is_empty());
}

/// A destroy that fails is retried next tick (the strike stays at/over
/// threshold) — the retry path through `reconcile_once`.
#[tokio::test(start_paused = true)]
async fn failed_destroy_retries_next_tick() {
    let host = scenario_host(0, 1).await;
    let sandbox_id = host.sandboxes[0].sandbox_id;

    host.revoke_ownership(0); // genuinely departed → orphan every tick
    host.reconcile.fail_next_destroy(); // the first destroy attempt fails

    let mut strikes = HashMap::new();
    host.reconcile_tick(&mut strikes).await.unwrap(); // strike 1
    host.reconcile_tick(&mut strikes).await.unwrap(); // strike 2 → destroy FAILS
    assert!(
        host.reconcile.is_live(sandbox_id),
        "a failed destroy leaves the sandbox live and the strike at threshold",
    );
    invariants::check(&host).await.unwrap();

    host.reconcile_tick(&mut strikes).await.unwrap(); // retry → destroy succeeds
    assert!(
        !host.reconcile.is_live(sandbox_id),
        "the next tick retries the destroy immediately",
    );
    invariants::check(&host).await.unwrap();
}

/// After a process CRASH, every local binding is lost (RAM died) while the FC
/// VMs survive; the successor's reconcile loop rebuilds every binding from the
/// coordinator and reaps nothing (the crash→restart→reconcile path that most
/// resembles the 2026-07-11 incident).
#[tokio::test(start_paused = true)]
async fn crash_loses_bindings_then_reconcile_repairs_all_without_reaping() {
    let mut host = scenario_host(0, 3).await;
    let ids: Vec<_> = host.sandboxes.iter().map(|s| s.sandbox_id).collect();

    host.crash_process().await.unwrap(); // RAM bindings die; FC set + coord survive
    for id in &ids {
        assert!(
            host.reconcile.binding(*id).is_none(),
            "crash clears bindings"
        );
        assert!(
            host.reconcile.is_live(*id),
            "FC VMs survive a host-agent crash"
        );
    }
    host.restart().await.unwrap();

    let mut strikes = HashMap::new();
    host.reconcile_tick(&mut strikes).await.unwrap();
    invariants::check(&host).await.unwrap();
    for id in &ids {
        assert!(
            host.reconcile.is_live(*id),
            "no owned sandbox reaped post-crash"
        );
        assert!(
            host.reconcile.binding(*id).is_some(),
            "every binding repaired from the coordinator",
        );
    }
    assert!(host.reconcile.destroyed().is_empty());
}

// ─────────────────── stale-binding slot TOCTOU (Flow B seam) ───────────────
//
// The portable NBD slot allocator's reserved-bit protocol is what closes the
// sweep-vs-survivor TOCTOU (`slot.rs::try_claim`). The dst-host scheduler is
// run-step-to-completion (no true concurrent futures), so the strongest
// deterministic version drives the two claim paths directly and asserts the
// two safety properties. A single-slot pool with `warm_target = 0` keeps the
// background populator idle (it never reserves), so the reserved bit is the
// only actor — fully deterministic.
//
// Deferred to P7 (Flow B extraction): the concurrent populator-vs-claim race
// under a randomized poll order — already covered by the host-agent's
// multi-thread `claim_wins_against_the_populators_validation_window` test.

#[tokio::test(start_paused = true)]
async fn stale_binding_sweep_never_double_claims_or_tears_a_live_binding() {
    use engram_host_agent::disk_daemon::slot::NbdSlotAllocator;
    use std::path::Path;

    let dev = Path::new("/dev/nbd0");

    // (A) No double-claim. The startup sweep reserves the device via
    // `try_claim`; while it holds the reservation across its (slow, sleeping)
    // DISCONNECT, a SECOND `try_claim` for the same device MUST fail — a live
    // binding can never be handed to two owners.
    {
        let pool = NbdSlotAllocator::with_capacity(1, 0);
        let sweep_guard = pool.try_claim(dev).await.expect("a free device reserves");
        assert!(
            pool.try_claim(dev).await.is_none(),
            "a device the sweep already reserved cannot be double-claimed",
        );
        drop(sweep_guard);
    }

    // (B) A live binding is never torn out. A survivor-rehydrate `claim` takes
    // the busy device (skipping the free-check, reserving it); the sweep's
    // `try_claim` then returns None → it SKIPS the device, so the survivor's
    // live binding is never disconnected out from under it. This is exactly
    // the TOCTOU `try_claim` closes.
    {
        let pool = NbdSlotAllocator::with_capacity(1, 0);
        let survivor_guard = pool
            .claim(dev)
            .await
            .expect("survivor claims the busy device");
        assert!(
            pool.try_claim(dev).await.is_none(),
            "the stale-binding sweep must skip a device a live lease holds",
        );
        drop(survivor_guard);
    }
}

// ───────────────────────── pinned swarm seeds ─────────────────────────
//
// A handful of chaos seeds run at full length: with ReconcileTick +
// DropLocalBinding + RevokeOwnership in the menu, these keep the acked-write
// AND reconcile None-arm oracles exercised across randomized interleavings
// forever. No known-bad seed today (the extraction is behaviour-preserving) —
// these are the standing regression net; a future sim-found failure gets its
// seed pinned here with the finding + fix.

async fn run_pinned(seed: u64, profile: Profile, steps: u64) {
    let mut sim = Sim::new(seed, profile).await;
    if let Err(msg) = sim.run(steps).await {
        let tail: Vec<_> = sim
            .report()
            .trace
            .iter()
            .rev()
            .take(25)
            .rev()
            .cloned()
            .collect();
        panic!("pinned seed {seed} ({profile:?}) regressed: {msg}\ntrace tail:\n{tail:#?}");
    }
}

#[tokio::test(start_paused = true)]
async fn pinned_chaos_seeds_hold_all_oracles() {
    // Modest budget (the per-op fsyncs make this I/O-bound; the broad net is
    // the release swarm `just sim-host-swarm 0..50 400`). Enough seeds to keep
    // several reconcile interleavings pinned forever.
    for seed in [0u64, 7, 13] {
        run_pinned(seed, Profile::Chaos, 200).await;
    }
}
