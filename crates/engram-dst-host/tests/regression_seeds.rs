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

use engram_dst_host::{
    decode_tag, invariants, CrashPoint, Profile, ScriptedResponse, Sim, SimHost, CHUNK_SIZE,
};
use engram_host_agent::disk_daemon::spool;

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

// ───────────── Flow A incident seeds (ADR 0098 P4 — the SIGTERM ladder) ────
//
// The three historical SIGTERM-path hazards, each reproduced as a
// deterministic scenario against the extracted ladder + the real
// spool/rebuild machinery, then pinned as proof the acked-write oracle armed
// over them.

/// #225 — the deadline overrun. A tiny final-flush budget overruns the
/// deadline, so the final-flush leg is SKIPPED: every survivor is a straggler
/// whose acked (un-published) dirty tier rides the shutdown spool — which is
/// NOT deadline-bound and always completes. The successor adopts it and
/// recovers every acked write. (The pre-spool code rolled these acked writes
/// back by up to a cadence window — the corruption the spool closed.)
#[tokio::test(start_paused = true)]
async fn sigterm_tiny_budget_overrun_spools_stragglers_and_recovers_every_acked_write() {
    let mut host = scenario_host(0, 3).await;
    // Acked writes that the final flush would upload if the deadline allowed.
    host.guest_write(0, 1).await.unwrap();
    host.guest_write(1, 2).await.unwrap();
    host.guest_write(2, 3).await.unwrap();
    assert!(host.ledger.len() >= 3);

    // 1 ms budget → the plan_shutdown deadline is below SIM_FLUSH_COST → the
    // final-flush leg overruns and is skipped; the spool is the ONLY copy.
    host.sigterm(Some(0.001)).await.unwrap();
    for slot in &host.sandboxes {
        let spooled = spool::read_spool(host.fs.spool_dir(), slot.sandbox_id)
            .await
            .unwrap();
        assert!(
            spooled.is_some_and(|(_, chunks)| !chunks.is_empty()),
            "an overrun straggler must leave a complete, chunk-bearing spool",
        );
    }

    // The successor restarts and adopts the spool → every acked write back.
    host.restart().await.unwrap();
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
    host.guest_read(0, 1).await.unwrap();
    host.guest_read(1, 2).await.unwrap();
    host.guest_read(2, 3).await.unwrap();
}

/// 85e0298a — the store-ahead recovery. The final flush uploads the chunks +
/// a new manifest to the store (durable), but the coordinator publish ack is
/// LOST — so coord's disk_manifest ref (the successor's rebuild ref) stays
/// STALE at the base. The abandon-sweep spool export writes the store-ahead
/// ref (zero-chunk, ref-only — spool.rs State 6). The successor MUST attach
/// from the spool's ahead ref, never roll back to coord's stale one.
#[tokio::test(start_paused = true)]
async fn store_ahead_lost_publish_ack_recovers_from_the_spool_ref_not_coords_stale_one() {
    let mut host = scenario_host(0, 1).await;
    host.guest_write(0, 0).await.unwrap();
    host.guest_write(0, 4).await.unwrap();

    // The final flush UPLOADS chunks + a v2 manifest to the store and advances
    // the backend's version — but the coord publish is lost, so the durable
    // pointer (`published_ref`, standing in for coord's ref) stays base-stale.
    let backend = host.sandboxes[0].backend.clone().unwrap();
    backend.flush().await.unwrap();
    let store_ahead = backend.manifest_ref().await;
    assert!(store_ahead.version > host.sandboxes[0].base_ref.version);
    assert!(
        host.sandboxes[0].published_ref.is_none(),
        "the publish ack was lost — coord never learned the store-ahead ref",
    );

    // The abandon sweep exports the ref-only store-ahead spool.
    host.spool_export(0).await.unwrap();
    let sid = host.sandboxes[0].sandbox_id;
    let (meta, chunks) = spool::read_spool(host.fs.spool_dir(), sid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        meta.manifest_ref(),
        store_ahead,
        "the spool carries the store-ahead ref coord never acked",
    );
    assert!(
        chunks.is_empty(),
        "ref-only spool: the chunks are already durable in the store",
    );

    // Crash + restart: the successor's rebuild ref is coord's stale base; the
    // store-ahead rule attaches from the spool's ahead ref instead.
    host.sandboxes[0].backend = None;
    host.restart().await.unwrap();
    assert_eq!(
        host.sandboxes[0].published_ref,
        Some(store_ahead),
        "the successor adopts the store-ahead ref, never rolls back to the stale one",
    );
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
    host.guest_read(0, 0).await.unwrap();
    host.guest_read(0, 4).await.unwrap();
}

/// #224 — insert-after-sweep. Once the ladder raises the terminal `abandoning`
/// flag (the [`Abandon`] stage), a late `create`/`rehydrate` completing in its
/// multi-second await window must abandon-in-place, NEVER leave a live NBD
/// data plane for process-exit `Drop` to netlink-disconnect the successor's
/// device. The dst-host scheduler is run-step-to-completion (no true
/// concurrent futures), and the literal insert races a `DashMap` holding a
/// Linux-only `NbdHandle` — so the strongest deterministic version the
/// portable surface allows is the extracted ORDERING CONTRACT: the pure
/// `admits_new_plane` gate flips exactly at `Abandon` and never re-opens. The
/// concrete drain-twice sweep + SeqCst flag + the `is_abandoning()` create-
/// reject / rehydrate abandon-in-place branches stay in the driver and are
/// owned by the FC lane.
///
/// [`Abandon`]: engram_host_core::ShutdownStage::Abandon
#[test]
fn insert_after_abandon_stage_is_routed_to_abandon_in_place() {
    use engram_host_core::{admits_new_plane, ShutdownStage};

    // Before Abandon, a completing insert may land its live data plane.
    for stage in [
        ShutdownStage::Signaled,
        ShutdownStage::TasksAborted,
        ShutdownStage::FinalFlush,
    ] {
        assert!(admits_new_plane(stage), "{stage:?} still admits new planes");
    }
    // From Abandon onward, a late insert MUST abandon-in-place.
    for stage in [
        ShutdownStage::Abandon,
        ShutdownStage::SpoolExport,
        ShutdownStage::Detached,
    ] {
        assert!(
            !admits_new_plane(stage),
            "{stage:?} must route a late insert to abandon-in-place",
        );
    }
    // The gate flips exactly at the Abandon boundary and never re-opens.
    let ladder = ShutdownStage::LADDER;
    let first_closed = ladder
        .iter()
        .position(|s| !admits_new_plane(*s))
        .expect("some stage closes the gate");
    assert_eq!(ladder[first_closed], ShutdownStage::Abandon);
    assert!(
        ladder[first_closed..].iter().all(|s| !admits_new_plane(*s)),
        "once closed, the gate stays closed for the rest of the ladder",
    );
}

/// The seeded crash-point injector, exhaustively: cut the process at every one
/// of the eight durable-operation boundaries (the H5 composition contract) and
/// prove the REAL recovery (rebuild + tolerant `read_spool` / `load_all`)
/// recovers every acked write — oracle #1 UNCONDITIONAL. The spool boundaries
/// model the final-flush leg completing (published tier covers the writes) and
/// a crash mid-spool-write (the spool is redundant → torn/absent safely
/// rejected); the persist boundaries prove reachability + tolerant recovery of
/// the durable_record format (Flow D wires them into the ledger in P5).
#[tokio::test(start_paused = true)]
async fn every_crash_point_injection_recovers_every_acked_write() {
    for cp in CrashPoint::ALL {
        let mut host = scenario_host(0, 2).await;
        // Acked writes at risk across the crash.
        host.guest_write(0, 0).await.unwrap();
        host.guest_write(1, 5).await.unwrap();

        host.crash_at(cp)
            .await
            .unwrap_or_else(|e| panic!("crash_at({cp:?}): {e}"));
        host.restart()
            .await
            .unwrap_or_else(|e| panic!("restart after {cp:?}: {e}"));
        invariants::check(&host)
            .await
            .unwrap_or_else(|v| panic!("crash point {cp:?}: {} — {}", v.invariant, v.detail));
    }
}

// ─────────────── P4.5: oracle honesty — the honest-loss boundary ───────────
//
// The adversarial-review finding: the old oracle demanded EVERY restarted
// acked write recover, which either never exercised the post-ack/pre-handoff
// hard-crash window or would falsely demand recovery of a write prod genuinely
// loses. The oracle now verifies the durability PIPELINE — a write is
// required-recoverable IFF it had a durable handoff (flush published OR spool
// captured) before the crash. These two seeds pin BOTH directions of that
// boundary.

/// The honest-loss direction: a guest write acked from the RAM dirty tier, then
/// ABRUPT process death (SIGKILL / power loss) BEFORE any flush or spool. The
/// write never had a durable handoff, so the successor's manifest legitimately
/// rolls the chunk back to base — an accepted, bounded loss (ADR 0028). The
/// honest oracle keys on the durable-handoff floor, so it recovers the floor
/// (here: base) and does NOT cry wolf demanding the un-handed-off acked tag.
#[tokio::test(start_paused = true)]
async fn post_ack_pre_handoff_crash_is_honest_loss() {
    // Deterministic pinned seed (only the entropy for ids; the scenario is
    // hand-driven, not pick-driven).
    let mut host = scenario_host(0xA55E_7717, 1).await;

    // A single acked write with a distinctive tag — NO FlushTick, NO SpoolExport.
    host.guest_write(0, 3).await.unwrap();
    let acked = host
        .ledger
        .latest_by_chunk()
        .get(&(0, 3))
        .expect("the write is acked in the ledger")
        .content_tag;
    assert!(acked > 0, "a real, distinctive acked tag");
    assert!(
        host.ledger.handed_off_tag(0, 3).is_none(),
        "no flush and no spool ⇒ the write never had a durable handoff",
    );

    // Abrupt death: the RAM dirty tier evaporates with NO shutdown spool.
    host.abrupt_crash().await.unwrap();
    // The successor rebuilds through the REAL recovery path (from_blob at the
    // durable pointer + tolerant spool adopt — here no spool, pointer = base).
    host.restart().await.unwrap();

    // The chunk legitimately reads BASE — the acked (un-handed-off) write is
    // gone, exactly as production loses it.
    let backend = host.sandboxes[0].backend.clone().unwrap();
    let got = decode_tag(&backend.read(3 * CHUNK_SIZE, CHUNK_SIZE).await.unwrap());
    assert_eq!(
        got, 0,
        "the manifest rolled back to base — the write is lost"
    );
    assert_ne!(
        got, acked,
        "prod loses this write; the oracle must not fake recovery"
    );

    // The honest oracle reports NO violation on this accepted loss.
    invariants::check(&host).await.unwrap_or_else(|v| {
        panic!(
            "the oracle cried wolf on an accepted loss: {} — {}",
            v.invariant, v.detail
        )
    });

    // The recoverable direction, in the SAME crash shape: a write that DID get a
    // durable handoff (a flush) then abrupt-crashed MUST recover — the pipeline
    // took responsibility for it.
    host.guest_write(0, 4).await.unwrap();
    let handed = host
        .ledger
        .latest_by_chunk()
        .get(&(0, 4))
        .unwrap()
        .content_tag;
    host.flush_tick(0).await.unwrap();
    assert_eq!(
        host.ledger.handed_off_tag(0, 4),
        Some(handed),
        "the flush published the write → its durable-handoff floor is recorded",
    );
    host.abrupt_crash().await.unwrap();
    host.restart().await.unwrap();
    let backend = host.sandboxes[0].backend.clone().unwrap();
    let got = decode_tag(&backend.read(4 * CHUNK_SIZE, CHUNK_SIZE).await.unwrap());
    assert_eq!(got, handed, "a handed-off write survives even abrupt death");
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
}

// ────────────── Flow B: the 731df805 scenario (ADR 0098 P7) ──────────────
//
// The headline device-lifecycle-ordering incident, pinned as a regression
// seed: a rung-2 PARKED survivor (paused VM, resident, `evicting`-shaped) whose
// NBD device the register-time rehydrate MISSED, so the stale-binding sweep
// disconnected its live rootfs and an un-pause landed on a dead data plane.
// Both the #739 defense (the local ChainHeadRecord pass re-serves it) and the
// un-pause data-plane gate (the last line, even when every list is wrong) are
// pinned.

/// #739 FIXED path: park → roll → register with the PRE-#739 buggy coord list
/// (omits parked survivors) but the #739 local ChainHeadRecord pass ON. The
/// local pass re-serves the parked survivor's device the coord list missed;
/// the stale sweep then skips it (served ⇒ claimed ⇒ not free-in-pool); the
/// un-pause serves the correct acked bytes.
#[tokio::test(start_paused = true)]
async fn park_roll_local_pass_reserves_survivor_sweep_skips_it_unpause_serves() {
    let mut host = scenario_host(0, 3).await;
    // Acked writes on the soon-to-be-parked survivor.
    host.guest_write(0, 1).await.unwrap();
    host.guest_write(0, 5).await.unwrap();

    // Rung-2 park sandbox 0 (evicting-shaped, VM resident).
    host.park(0);
    assert!(host.sandboxes[0].parked);
    let gen_before = host.generation;

    // The pod roll: spool the survivors, drop RAM, fresh generation. The parked
    // VM stays resident; its kernel device is left bound to the dead generation.
    host.crash_process().await.unwrap();
    assert_eq!(host.generation, gen_before + 1);
    assert!(
        host.sandboxes[0].parked,
        "the parked VM stays resident across the roll"
    );
    assert_eq!(
        host.sandboxes[0].served_by, None,
        "the roll leaves the device unserved (the successor's serve socket)"
    );
    assert!(
        host.sandboxes[0]
            .kernel_owner
            .is_some_and(|g| g < host.generation),
        "the kernel device is still bound to the dead generation",
    );

    // Register-time rehydrate: the buggy coord list OMITS the parked survivor,
    // but the #739 local ChainHeadRecord pass catches it.
    host.register_rehydrate(
        /*coord_includes_parked=*/ false, /*local_pass_enabled=*/ true,
    )
    .await
    .unwrap();

    assert_eq!(
        host.sandboxes[0].served_by,
        Some(host.generation),
        "the #739 local pass re-served the parked survivor's device the coord list missed",
    );
    assert_eq!(
        host.sandboxes[0].kernel_owner,
        Some(host.generation),
        "the stale-binding sweep did NOT disconnect the re-served live device",
    );

    // The oracles hold and the un-pause serves the correct (acked) bytes.
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
    assert!(host.unpause(0), "the un-pause serves the live plane");
    assert!(!host.sandboxes[0].parked);
    host.guest_read(0, 1).await.unwrap();
    host.guest_read(0, 5).await.unwrap();
    invariants::check(&host).await.unwrap();
}

/// UNGATED path: park → roll → register with BOTH the buggy coord list AND the
/// #739 local pass DISABLED (the pre-#739 world). Nothing re-serves the parked
/// survivor, and the stale-binding sweep DISCONNECTS its live rootfs (the
/// literal 731df805 bug). The un-pause data-plane gate is then the LAST LINE:
/// it fails fast into `evict_local → resume` rather than serving the dead
/// plane, so the guest stays parked and NO oracle fires — the corruption never
/// happens.
#[tokio::test(start_paused = true)]
async fn park_roll_ungated_local_pass_off_unpause_gate_fires_never_dead_plane() {
    let mut host = scenario_host(0, 3).await;
    host.guest_write(0, 2).await.unwrap();
    host.park(0);
    host.crash_process().await.unwrap();

    // The pre-#739 world: buggy coord list + no local pass.
    host.register_rehydrate(false, false).await.unwrap();
    assert_eq!(
        host.sandboxes[0].served_by, None,
        "no pass re-served the parked survivor",
    );
    assert_eq!(
        host.sandboxes[0].kernel_owner, None,
        "the stale sweep disconnected the parked survivor's live device (the 731df805 bug)",
    );

    // The un-pause gate fires: the guest is NOT un-paused onto the dead plane.
    assert!(
        !host.unpause(0),
        "the un-pause data-plane gate must fire on an unserved device",
    );
    assert!(
        host.sandboxes[0].parked,
        "the guest stays parked, routed to evict_local → resume",
    );
    // Crucially: the corruption never happens, so the oracles are clean.
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
}

/// Scheduler-driven slot accounting (ADR 0098 P7): interleave `SlotClaim`
/// (Free → Claimed via the real `try_claim`) and `SlotPopulateTick`
/// (Claimed → Free via the lease `Drop`/`release`) over the REAL allocator,
/// asserting the accounting identity + no-double-claim hold at every step.
///
/// The TIGHT concurrent claim-vs-populate validation-window race stays the
/// multi-thread host-agent test `claim_wins_against_the_populators_validation_window`
/// (the concurrency-level complement): paused single-thread tokio can't hold a
/// `claim` mid-populate against a `with_capacity` pool's fast free-check, and
/// `claim`'s retry `sleep` would hang on the paused clock — so the sim drives
/// the transitions as explicit scheduler steps and the oracle proves accounting.
#[tokio::test(start_paused = true)]
async fn scheduler_driven_slot_accounting_holds_across_claim_release_interleavings() {
    let mut host = scenario_host(0, 3).await;
    // Both spare devices claimed (Free → Claimed).
    host.slot_claim(0).await;
    host.slot_claim(1).await;
    invariants::check(&host).await.unwrap();

    // A second claim of a held spare must NOT double-claim (the reserved-bit
    // protocol refuses it synchronously).
    let held_before = host.leases_held();
    host.slot_claim(0).await;
    assert_eq!(
        host.leases_held(),
        held_before,
        "a held device is never handed to a second owner",
    );
    invariants::check(&host).await.unwrap();

    // Release the oldest spare (Claimed → Free) then re-claim — accounting
    // stays exact across the interleaving.
    host.slot_populate_tick();
    invariants::check(&host).await.unwrap();
    host.slot_claim(0).await;
    invariants::check(&host).await.unwrap();
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
