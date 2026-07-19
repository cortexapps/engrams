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
    decode_tag, invariants, synth_chunk, CrashFs, Profile, ScriptedResponse, Sim, SimHost,
    CHUNK_SIZE, SIM_FINALIZE_MAX_ATTEMPTS,
};
use engram_host_agent::disk_daemon::spool;
use engram_host_core::TokioFs;

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
        let spooled = spool::read_spool(&TokioFs, host.fs.spool_dir(), slot.sandbox_id)
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
    let (meta, chunks) = spool::read_spool(&TokioFs, host.fs.spool_dir(), sid)
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

/// The op-boundary spool injector, exhaustively (P5, replacing the P4
/// static-boundary version): the predecessor's spool write is cut at EVERY
/// fs-op index of the real `write_spool` sequence by the `CrashFs` seam.
/// The acked set is redundantly flush-published first, so whatever the cut
/// leaves — the intact prior spool, no spool, a marker-less partial, or a
/// complete rewrite — the REAL recovery (rebuild + tolerant `read_spool`)
/// recovers EVERY acked write. The crash schedule is derived from the
/// production op trace, never a parallel list (`crashpoint_coverage.rs`).
#[tokio::test(start_paused = true)]
async fn spool_cut_at_every_op_recovers_every_acked_write() {
    // Derive the schedule length from one un-cut run of the same shape.
    let probe = {
        let mut host = scenario_host(0, 2).await;
        host.guest_write(0, 0).await.unwrap();
        let backend = host.sandboxes[0].backend.clone().unwrap();
        let (exported_ref, chunks) = backend.export_unflushed().await;
        let fs = CrashFs::recording();
        spool::write_spool(
            fs.as_ref(),
            host.fs.spool_dir(),
            host.sandboxes[0].sandbox_id,
            exported_ref,
            &chunks,
        )
        .await
        .unwrap();
        fs.trace().len()
    };
    assert!(probe > 0, "the probe run must trace a real op sequence");

    for op_index in 0..=probe {
        let mut host = scenario_host(0, 2).await;
        // Acked writes at risk across the crash (sandbox 0 is the cut
        // target; sandbox 1 proves uninvolved sandboxes ride through).
        host.guest_write(0, 0).await.unwrap();
        host.guest_write(1, 5).await.unwrap();

        host.spool_crash_at(op_index)
            .await
            .unwrap_or_else(|e| panic!("spool_crash_at({op_index}): {e}"));
        host.restart()
            .await
            .unwrap_or_else(|e| panic!("restart after cut {op_index}: {e}"));
        invariants::check(&host)
            .await
            .unwrap_or_else(|v| panic!("cut {op_index}: {} — {}", v.invariant, v.detail));
        // Every acked write recovers: the floor was raised to the acked tags
        // before the cut, so the honest range pins the exact bytes.
        host.guest_read(0, 0).await.unwrap();
        host.guest_read(1, 5).await.unwrap();
    }
}

// ───────────────────── Flow D: eviction finalize (ADR 0098 P5) ─────────────

/// The graceful finalize completes and RAISES the published floor: begin →
/// tick to terminal → the acked writes now ride the finalize-published disk
/// manifest, the terminal `EvictionFinal` record exists, the finalize record
/// is gone, and the destroy was issued. A restart then rebuilds from the
/// finalize-published ref and every acked write reads back exactly.
#[tokio::test(start_paused = true)]
async fn finalize_completes_publishes_the_floor_and_destroys() {
    let mut host = scenario_host(0, 2).await;
    host.guest_write(0, 1).await.unwrap();
    host.guest_write(0, 6).await.unwrap();

    let engram_dst_host::CaptureOutcome::Began(snapshot_id) = host.snapshot_begin(0).await.unwrap()
    else {
        panic!("expected a fresh finalize to begin");
    };
    assert!(
        host.sandboxes[0].backend.is_none(),
        "the captured VM is paused for the whole finalize",
    );
    // One tick completes every leg (disk → memory → blobs → terminal).
    host.finalize_tick(0).await.unwrap();

    assert!(
        host.pending_finalizes.is_empty(),
        "terminal cleared the map"
    );
    assert_eq!(
        host.destroyer.destroyed(),
        vec![host.sandboxes[0].sandbox_id],
        "the terminal leg issued the (best-effort) destroy",
    );
    let published = host.sandboxes[0]
        .published_ref
        .expect("the finalize disk manifest is the durable pointer");
    assert!(published.version > host.sandboxes[0].base_ref.version);

    // The floor was raised: a restart rebuilds from the finalize-published
    // manifest and the acked writes read back exactly (floor == latest).
    host.restart().await.unwrap();
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
    host.guest_read(0, 1).await.unwrap();
    host.guest_read(0, 6).await.unwrap();
    invariants::check_finalize_convergence(&host)
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
    let _ = snapshot_id;
}

/// Crash between EVERY pair of finalize fs-ops, then resume: for each op
/// index, one attempt runs under a `CrashFs` cut, the process dies, and the
/// restart re-drives from the persisted stage through the REAL `load_all` +
/// attempt loop to completion. Oracle #6 (stage monotone + stage⇒fields)
/// holds at every step; the terminal manifests and the raised floor are
/// invariant to WHERE the crash landed (idempotent redrive).
#[tokio::test(start_paused = true)]
async fn finalize_crash_at_every_op_resumes_and_completes() {
    // A generous bound on the finalize pass's op count (the pass ends early
    // at the cut anyway; past-the-end cuts complete before dying).
    const MAX_OPS: usize = 40;
    for op_index in 0..MAX_OPS {
        let mut host = scenario_host(0, 2).await;
        host.guest_write(0, 2).await.unwrap();
        host.guest_write(0, 7).await.unwrap();

        assert!(matches!(
            host.snapshot_begin(0).await.unwrap(),
            engram_dst_host::CaptureOutcome::Began(_)
        ));
        host.finalize_crash_at(0, op_index)
            .await
            .unwrap_or_else(|e| panic!("finalize_crash_at({op_index}): {e}"));

        // The successor: resume the durable record (if the attempt completed
        // before the cut index, there is nothing to resume) and drive the
        // remaining attempts to convergence.
        host.restart().await.unwrap();
        for _ in 0..=SIM_FINALIZE_MAX_ATTEMPTS {
            if host.pending_finalizes.is_empty() {
                break;
            }
            host.finalize_tick(0).await.unwrap();
        }
        invariants::check(&host)
            .await
            .unwrap_or_else(|v| panic!("cut {op_index}: {} — {}", v.invariant, v.detail));
        invariants::check_finalize_convergence(&host)
            .unwrap_or_else(|v| panic!("cut {op_index}: {} — {}", v.invariant, v.detail));
        // Completed (never quarantined): one cut costs at most one attempt,
        // and the healed successor completes on its first. The acked writes
        // ride the finalize-published floor — crash placement is invisible.
        let published = host.sandboxes[0]
            .published_ref
            .unwrap_or_else(|| panic!("cut {op_index}: the finalize must have published"));
        assert!(published.version > host.sandboxes[0].base_ref.version);
        host.restart().await.unwrap();
        host.guest_read(0, 2).await.unwrap();
        host.guest_read(0, 7).await.unwrap();
    }
}

/// A finalize whose leg input is permanently gone quarantines and stays
/// convergent: the staging `disk-pending/` files vanish (the "finding 1"
/// ENOENT class — the input that lives only in the staging dir), so every
/// attempt's disk leg fails while the record machinery stays healthy.
/// Attempts exhaust → the record lands in `finalize/failed/`, the
/// idempotency map clears, and the honest floor stays at the PRIOR
/// published tier (the bounded rollback the quarantine doc promises) — the
/// oracle does not demand the un-published writes back.
#[tokio::test(start_paused = true)]
async fn finalize_quarantine_after_max_attempts_is_convergent() {
    let mut host = scenario_host(0, 2).await;
    // A prior flush establishes the floor the quarantine falls back to.
    host.guest_write(0, 3).await.unwrap();
    host.flush_tick(0).await.unwrap();
    let prior_published = host.sandboxes[0].published_ref.expect("flushed");
    // A newer acked write that will ride the (doomed) finalize.
    host.guest_write(0, 3).await.unwrap();

    let engram_dst_host::CaptureOutcome::Began(snapshot_id) = host.snapshot_begin(0).await.unwrap()
    else {
        panic!("expected a fresh finalize to begin");
    };
    // The staging inputs vanish out from under the record.
    let pending_dir = host
        .fs
        .root()
        .join("staging")
        .join(snapshot_id.to_string())
        .join("disk-pending");
    tokio::fs::remove_dir_all(&pending_dir).await.unwrap();

    // Every attempt fails its disk leg; the ladder exhausts to quarantine.
    for _ in 0..SIM_FINALIZE_MAX_ATTEMPTS {
        host.finalize_tick(0).await.unwrap();
        invariants::check(&host)
            .await
            .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
    }
    assert!(
        host.pending_finalizes.is_empty(),
        "quarantine must clear the idempotency map",
    );
    let failed_marker = host
        .fs
        .root()
        .join("finalize")
        .join("failed")
        .join(format!("{snapshot_id}.json"));
    assert!(
        failed_marker.exists(),
        "the quarantined record is kept for operator forensics, never dropped",
    );
    invariants::check_finalize_convergence(&host)
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));

    // The honest floor: a restart rebuilds from the PRIOR published tier;
    // the newer un-published write is the accepted bounded rollback.
    host.restart().await.unwrap();
    assert_eq!(
        host.sandboxes[0].published_ref,
        Some(prior_published),
        "quarantine must not move the durable pointer",
    );
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
    host.guest_read(0, 3).await.unwrap();
}

/// The capture-lock lockout's observable contract: a second `snapshot_begin`
/// on a sandbox with a pending finalize re-observes the SAME snapshot id —
/// no second record, no second job, no concurrent chain mutation (the real
/// `pending_finalizes.get` guard at the entry). Completion releases it: a
/// LATER begin on the (restarted) sandbox mints a fresh id.
#[tokio::test(start_paused = true)]
async fn snapshot_begin_idempotent_under_pending_finalize() {
    let mut host = scenario_host(0, 2).await;
    host.guest_write(0, 4).await.unwrap();

    let engram_dst_host::CaptureOutcome::Began(first) = host.snapshot_begin(0).await.unwrap()
    else {
        panic!("expected a fresh finalize to begin");
    };
    let engram_dst_host::CaptureOutcome::AlreadyPending(second) =
        host.snapshot_begin(0).await.unwrap()
    else {
        panic!("expected the pending finalize to be re-observed");
    };
    assert_eq!(first, second, "a pending finalize re-observes the same id");
    assert_eq!(
        host.finalize_started.len(),
        1,
        "no second finalize was started",
    );

    // Terminal completes and releases the lockout; a fresh capture (after
    // the sandbox is rebuilt/resumed) mints a fresh snapshot.
    host.finalize_tick(0).await.unwrap();
    host.restart().await.unwrap();
    host.guest_write(0, 4).await.unwrap();
    let engram_dst_host::CaptureOutcome::Began(third) = host.snapshot_begin(0).await.unwrap()
    else {
        panic!("expected a fresh begin after completion");
    };
    assert_ne!(third, first, "a completed finalize does not pin the id");
    // Drain to keep the scenario convergent.
    host.finalize_tick(0).await.unwrap();
    invariants::check_finalize_convergence(&host)
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
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

#[tokio::test(start_paused = true)]
async fn misdirected_read_in_range_tag_fires_the_membership_oracle() {
    let mut host = scenario_host(0xA55E_7718, 3).await;
    host.guest_write(0, 0).await.unwrap();
    host.guest_write(0, 1).await.unwrap();
    host.guest_write(0, 0).await.unwrap();

    host.sandboxes[0]
        .backend
        .clone()
        .unwrap()
        .write(0, &synth_chunk(2))
        .await
        .unwrap();

    let violation = invariants::check(&host)
        .await
        .expect_err("the in-range tag belongs to another chunk");
    assert_eq!(violation.invariant, "acked-write-durability");
    assert!(violation.detail.contains("never acked"), "{violation:?}");
}

#[tokio::test(start_paused = true)]
async fn unflushed_acked_write_fires_the_quiescent_floor_oracle() {
    let mut host = scenario_host(0xA55E_7719, 3).await;
    host.guest_write(0, 0).await.unwrap();

    invariants::check(&host).await.unwrap();
    let violation = invariants::check_quiescent_floor(&host)
        .await
        .expect_err("a surviving unflushed ack must fail the quiescence-only oracle");
    assert_eq!(violation.invariant, "quiescent-floor");

    host.flush_tick(0).await.unwrap();
    invariants::check_quiescent_floor(&host).await.unwrap();
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

/// R6 headline (ADR 0098 §Phase 3, #784 layer 1 / #769 gap A): the UNGATED
/// path — park → roll → register with BOTH the buggy coord list AND the #739
/// local pass DISABLED (every upstream rehydrate misses the survivor). The
/// survivor's FC guest is STILL LIVE and holds its rootfs device open, so the
/// stale-binding sweep now PARKS (proof of death not met) instead of
/// DISCONNECTing. The device is left RECONNECTABLE, and a later reattach pass
/// (a corrected coord list) re-serves it with ZERO loss.
///
/// This is the exact 2026-07-18/19 firing turned into a non-event. Fail-without
/// / pass-with proof: revert `sweep_verdict` to the pre-R6 form (dead-owner ⇒
/// Disconnect regardless of holder) and the sweep clears `kernel_owner` under a
/// live holder → the `severed-live-holder` oracle fires here (the old seed's
/// disconnect returns). With the Park guard, `kernel_owner` survives, the oracle
/// holds, and the re-serve is lossless.
#[tokio::test(start_paused = true)]
async fn park_roll_ungated_live_holder_sweep_parks_then_reattach_reserves_zero_loss() {
    let mut host = scenario_host(0, 3).await;
    host.guest_write(0, 2).await.unwrap();
    host.park(0);
    host.crash_process().await.unwrap();
    assert!(
        host.sandboxes[0].guest_holds_device,
        "the survivor's FC guest survives the roll and still holds its device open",
    );

    // The pre-#739 world: buggy coord list + no local pass. Nothing re-serves
    // the parked survivor, so its dead-owner device reaches the stale sweep.
    host.register_rehydrate(false, false).await.unwrap();
    assert_eq!(
        host.sandboxes[0].served_by, None,
        "no pass re-served the parked survivor",
    );
    // The R6 change: the sweep PARKED the live-held device instead of
    // disconnecting it — the kernel binding is intact (RECONNECTABLE).
    assert!(
        host.sandboxes[0]
            .kernel_owner
            .is_some_and(|g| g < host.generation),
        "the stale sweep PARKED the live-held survivor device (kernel binding intact), \
         not disconnected — the #769 gap-A guard",
    );
    // The severed-live-holder oracle holds: a live guest's device is never left
    // both unserved AND unbound.
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));

    // A later reattach pass with a corrected coord list re-serves the SAME
    // device — the guest kept reading it the whole time, so the acked bytes are
    // served with zero loss.
    host.register_rehydrate(true, true).await.unwrap();
    assert_eq!(
        host.sandboxes[0].served_by,
        Some(host.generation),
        "the corrected reattach pass re-served the parked survivor's device",
    );
    assert_eq!(
        host.sandboxes[0].kernel_owner,
        Some(host.generation),
        "the re-served device is bound to the current generation",
    );
    assert!(
        host.unpause(0),
        "the un-pause now serves the live re-served plane"
    );
    host.guest_read(0, 2).await.unwrap();
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
}

/// The un-pause gate's own coverage, kept honest under R6: a survivor whose FC
/// guest is GENUINELY GONE (crashed/destroyed) no longer holds its device open,
/// so the stale sweep sees `NoHolder` and a DISCONNECT is LEGAL (proof of death
/// met — there is no live guest to protect). If a stale coordinator then
/// attempts a late un-pause onto that now-disconnected device, the un-pause
/// data-plane gate is still the last line: it fails fast into `evict_local →
/// resume` rather than serving a dead plane, and no oracle fires.
#[tokio::test(start_paused = true)]
async fn park_roll_guest_gone_noholder_disconnect_legal_unpause_gate_still_guards() {
    let mut host = scenario_host(0, 3).await;
    host.guest_write(0, 2).await.unwrap();
    host.park(0);
    host.crash_process().await.unwrap();
    // The guest genuinely died after the roll — nothing holds the device open.
    host.kill_guest(0);

    // Pre-#739 world again, but this time the sweep has proof of death.
    host.register_rehydrate(false, false).await.unwrap();
    assert_eq!(
        host.sandboxes[0].served_by, None,
        "no pass re-served the survivor",
    );
    assert_eq!(
        host.sandboxes[0].kernel_owner, None,
        "the sweep legally DISCONNECTED a dead-owner device with no live holder (NoHolder)",
    );

    // A late un-pause is still refused by the data-plane gate — the guest is
    // NOT un-paused onto the dead plane.
    assert!(
        !host.unpause(0),
        "the un-pause data-plane gate must still fire on an unserved device",
    );
    assert!(
        host.sandboxes[0].parked,
        "the guest stays parked, routed to evict_local → resume",
    );
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

// ───────── G2: the survivor-invisibility family — capture + resume ─────────
//
// Two incidents in two days shared one mechanism: a lookup keyed on a
// tracking map a post-roll survivor isn't in, causing a silent skip that
// corrupts. 731df805 (#739) was register/sweep — pinned above. 03e6535e
// (#743) was capture (`nbd_sandboxes` missing → recoverable snapshot with
// disk_manifest=None) and resume (boot onto the capture-time literal
// /dev/nbdN). These seeds pin all four legs' shared shape through the pure
// verdicts (`plan_capture_disk_drain` / `plan_resume_attach`) the prod
// guards and the sim both drive.

/// FIXED capture leg: a post-roll resident survivor (VM there, disk server
/// never rehydrated) is REFUSED — never silently skipped into a
/// manifestless snapshot. Rehydrating it (the error message's remediation)
/// makes the capture drain normally.
#[tokio::test(start_paused = true)]
async fn untracked_survivor_capture_refuses_never_a_manifestless_snapshot() {
    let mut host = scenario_host(0, 2).await;
    host.guest_write(0, 1).await.unwrap();
    host.flush_tick(0).await.unwrap();
    host.guest_write(0, 1).await.unwrap(); // newer, un-published ack

    // The roll: RAM dies, the VM stays resident, nothing rehydrates it.
    host.abrupt_crash().await.unwrap();
    assert!(host.sandboxes[0].backend.is_none());

    let outcome = host.snapshot_begin(0).await.unwrap();
    assert_eq!(
        outcome,
        engram_dst_host::CaptureOutcome::RefusedUntracked,
        "an untracked resident survivor must be refused, not skipped",
    );
    assert!(
        host.pending_finalizes.is_empty() && host.finalize_started.is_empty(),
        "a refusal records nothing",
    );
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));

    // Remediation: rehydrate (restart rebuilds = the nbd_sandboxes entry
    // returns), then the capture drains normally.
    host.restart().await.unwrap();
    assert!(matches!(
        host.snapshot_begin(0).await.unwrap(),
        engram_dst_host::CaptureOutcome::Began(_)
    ));
    host.finalize_tick(0).await.unwrap();
    invariants::check_finalize_convergence(&host)
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
}

/// UNGATED capture + UNGATED resume — the literal #743 double failure, and
/// the proof the acked-write oracle CATCHES it: the pre-#743 silent-skip
/// capture completes a finalize with disk_manifest=None (poisoned lineage;
/// the floor stays where the last real flush put it), and the pre-#743
/// resume boots FC onto the capture-time literal device (base content).
/// The published-floor writes are gone from what the guest reads — the
/// oracle MUST fire the 85e0298a/03e6535e below-floor violation.
#[tokio::test(start_paused = true)]
async fn ungated_capture_and_resume_poison_the_lineage_and_the_oracle_catches_it() {
    let mut host = scenario_host(0, 2).await;
    host.guest_write(0, 2).await.unwrap();
    host.flush_tick(0).await.unwrap(); // floor raised over the acked write

    host.abrupt_crash().await.unwrap();

    // Pre-#743 capture: the silent skip records a disk-less finalize.
    let _poisoned = host.snapshot_begin_pre743(0).await.unwrap();
    host.finalize_tick(0).await.unwrap();
    assert!(host.pending_finalizes.is_empty(), "the finalize completed");
    assert!(
        host.sandboxes[0].poisoned_snapshot,
        "a completed finalize without a disk manifest is the poisoned lineage",
    );

    // Pre-#743 resume: boots onto the stale literal device.
    let outcome = host.resume_finalized(0, /*gated=*/ false).await.unwrap();
    assert_eq!(outcome, engram_dst_host::ResumeOutcome::BootedStaleLiteral);

    // The oracle catches the corruption: the flushed (published-floor)
    // write is below-floor gone from what the stale device serves.
    let violation = invariants::check(&host)
        .await
        .expect_err("the acked-write oracle must catch the stale-literal boot");
    assert_eq!(violation.invariant, "acked-write-durability");
    assert!(
        violation.detail.contains("rolled back below the floor"),
        "the 03e6535e corruption is the below-floor class: {}",
        violation.detail,
    );
}

/// GATED resume as the last line: even with the poisoned snapshot already
/// manufactured (the capture gate bypassed), the resume gate refuses the
/// stale literal — no boot, no corruption, oracles clean. The exact
/// defense-in-depth shape of the 731df805 un-pause-gate seed.
#[tokio::test(start_paused = true)]
async fn poisoned_snapshot_resume_gate_refuses_the_stale_literal() {
    let mut host = scenario_host(0, 2).await;
    host.guest_write(0, 3).await.unwrap();
    host.flush_tick(0).await.unwrap();
    host.abrupt_crash().await.unwrap();

    let _poisoned = host.snapshot_begin_pre743(0).await.unwrap();
    host.finalize_tick(0).await.unwrap();
    assert!(host.sandboxes[0].poisoned_snapshot);

    let outcome = host.resume_finalized(0, /*gated=*/ true).await.unwrap();
    assert_eq!(
        outcome,
        engram_dst_host::ResumeOutcome::RefusedStaleLiteral,
        "the resume gate must refuse the stale literal device",
    );
    assert!(
        host.sandboxes[0].backend.is_none(),
        "no boot happened — the guest never lands on the dead plane",
    );
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
}

// ───────────── Flow F: the flush-pipeline interleavings (ADR 0098 P6) ─────

/// #204 through the sim + ledger: a REAL flush parked at the dirty→pending
/// handoff races a guest read AND a guest write of the drained chunk. The
/// step's internal assertion pins the read to {drained, racing} — never
/// pre-drain stale base — and the standing oracle then proves the racing
/// write survives the published floor (the pre-fix code silently shadowed
/// the drained bytes via a stale-base RMW).
#[tokio::test(start_paused = true)]
async fn flush_handoff_race_serves_drained_truth_and_loses_no_write() {
    let mut host = scenario_host(0, 2).await;
    host.flush_handoff_race(0).await.unwrap();
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
    // The racing write is now the latest ack; a follow-up flush publishes
    // it and the floor pins it forever.
    host.flush_tick(0).await.unwrap();
    host.guest_read(0, 0).await.unwrap();
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
}

/// #199's fence leg: the migration fence rises while a REAL flush is
/// parked post-upload/pre-publish. The publish must abort (manifest
/// unmoved, chunks_flushed 0, dirty re-queued — asserted inside the step),
/// the ledger floor never moves on the aborted attempt, and the post-heal
/// flush publishes the re-queued writes.
#[tokio::test(start_paused = true)]
async fn fence_raised_mid_upload_aborts_publish_and_requeues() {
    let mut host = scenario_host(0, 2).await;
    host.flush_fence_abort(0).await.unwrap();
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
    // The step's healing tail already re-flushed; the write is on the floor.
    host.guest_read(0, 1).await.unwrap();
}

/// #199's ordering leg: a SECOND flush launched while an earlier one is
/// parked mid-pipeline must serialize behind it (the flush-pipeline
/// guard), never publish out of order. The final published content is the
/// NEWER write — an old-over-new overwrite would put the floor above what
/// the device serves and the oracle would fire.
#[tokio::test(start_paused = true)]
async fn concurrent_flushes_serialize_never_reorder_publishes() {
    use engram_host_agent::disk_daemon::backend::FlushSeamPoint;
    let mut host = scenario_host(0, 1).await;
    host.guest_write(0, 3).await.unwrap();
    let backend = host.sandboxes[0].backend.clone().unwrap();

    // Flush A drains the first write and parks post-upload/pre-publish.
    let (arrived, proceed) = backend.arm_flush_seam(FlushSeamPoint::PostUploadPrePublish);
    let a_backend = backend.clone();
    let flush_a = tokio::spawn(async move { a_backend.flush().await });
    arrived.notified().await;

    // A NEWER guest write lands, and flush B starts — it must block on the
    // pipeline guard until A completes (drain order == publish order).
    host.guest_write(0, 3).await.unwrap();
    let b_backend = backend.clone();
    let flush_b = tokio::spawn(async move { b_backend.flush().await });
    tokio::task::yield_now().await;

    proceed.notify_one();
    flush_a.await.unwrap().unwrap();
    flush_b.await.unwrap().unwrap();
    host.note_flush_published(0).await.unwrap();

    // The floor and the served content are the NEWER tag; the standing
    // oracle would flag an old-over-new publish as a below-floor read.
    host.guest_read(0, 3).await.unwrap();
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
}

/// The pre-rebase store-ahead crash window: the process dies with a REAL
/// flush parked between `put_manifest` and the rebase. The store holds a
/// manifest nothing references; the floor never rose, so the successor's
/// rollback is HONEST — and its next flush recovers through the REAL
/// version-conflict retry (attempts the stale next-version, hits the
/// conflict, re-targets latest+1).
#[tokio::test(start_paused = true)]
async fn pre_rebase_crash_is_honest_and_the_conflict_retry_recovers() {
    let mut host = scenario_host(0, 2).await;
    // An earlier real flush establishes a floor the crash must not breach.
    host.guest_write(0, 4).await.unwrap();
    host.flush_tick(0).await.unwrap();

    // The crash: a newer write's flush dies published-but-unrebased.
    host.flush_pre_rebase_crash(0).await.unwrap();
    host.restart().await.unwrap();
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
    host.guest_read(0, 4).await.unwrap();
    host.guest_read(0, 2).await.unwrap();

    // Recovery: the successor's next flush hits the store-ahead version
    // conflict (the orphaned manifest occupies next-version) and the REAL
    // retry loop re-targets past it.
    host.guest_write(0, 5).await.unwrap();
    host.flush_tick(0).await.unwrap();
    host.guest_read(0, 5).await.unwrap();
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
}

// ───────────────── Flow E: the migration decision table (ADR 0098 P8) ─────

/// An expired export whose state.bin never shipped aborts IN PLACE with
/// zero loss: the guest un-freezes and every acked write is exactly where
/// it was (the dirty tier never left the live backend).
#[tokio::test(start_paused = true)]
async fn ttl_expired_unshipped_export_aborts_in_place_zero_loss() {
    let mut host = scenario_host(0, 2).await;
    host.guest_write(0, 1).await.unwrap();
    assert!(host.migration_begin(0).await.unwrap());
    assert!(host.sandboxes[0].migrating);
    // Frozen: the guest acks nothing while exporting.
    let acked_before = host.ledger.len();
    host.guest_write(0, 2).await.unwrap();
    assert_eq!(
        host.ledger.len(),
        acked_before,
        "a frozen source acks nothing"
    );

    // The TTL elapses with no serving activity and no commit/abort.
    tokio::time::advance(std::time::Duration::from_secs(150)).await;
    host.migration_ttl_sweep().await.unwrap();

    assert!(
        !host.sandboxes[0].migrating,
        "un-shipped export aborts in place"
    );
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
    host.guest_read(0, 1).await.unwrap();
    // The guest is live again: a new write acks.
    host.guest_write(0, 2).await.unwrap();
    assert_eq!(host.ledger.len(), acked_before + 1);
}

/// The #216 core: once state.bin has shipped, the expired export STAYS
/// PAUSED under `ownership == true` (an un-pause would be the split-brain
/// — oracle #7's catch), and only an explicit ownership flip to `false`
/// lets the next sweep destroy the corpse. Never a resume.
#[tokio::test(start_paused = true)]
async fn state_served_export_never_unpauses_then_destroys_on_ownership_flip() {
    let mut host = scenario_host(0, 2).await;
    host.guest_write(0, 3).await.unwrap();
    host.flush_tick(0).await.unwrap(); // the shipped state's floor
    assert!(host.migration_begin(0).await.unwrap());
    host.migration_serve_state(0); // the split-brain moment

    tokio::time::advance(std::time::Duration::from_secs(150)).await;
    host.migration_ttl_sweep().await.unwrap();
    assert!(
        host.sandboxes[0].migrating,
        "a served export must STAY PAUSED while the coordinator says we own it",
    );
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));

    // The scanner rehomes the session (lease expired): ownership flips
    // false — the NEXT sweep destroys the frozen corpse, never resuming it.
    host.coord.revoke_owner(host.sandboxes[0].sandbox_id);
    tokio::time::advance(std::time::Duration::from_secs(150)).await;
    host.migration_ttl_sweep().await.unwrap();
    assert!(
        !host.sandboxes[0].migrating,
        "ownership moved on — destroyed"
    );
    assert!(
        host.sandboxes[0].backend.is_none(),
        "the stale source is torn down, never resumed",
    );
    assert!(
        host.split_brain_unpauses.is_empty(),
        "no abort-unpause ever touched the served export (#216)",
    );
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
}

/// #216 Gap 1's standing form: an export that keeps SERVING never
/// expires, however far past the TTL its creation slips — the activity
/// anchor, not `created_at`, is the clock.
#[tokio::test(start_paused = true)]
async fn actively_serving_export_never_expires_mid_transfer() {
    let mut host = scenario_host(0, 2).await;
    host.guest_write(0, 4).await.unwrap();
    assert!(host.migration_begin(0).await.unwrap());

    // 5 × 60 s = 300 s of transfer (>> EXPORT_TTL = 120 s), each minute
    // touched by a page serve.
    for _ in 0..5 {
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        host.migration_touch(0);
        host.migration_ttl_sweep().await.unwrap();
        assert!(
            host.sandboxes[0].migrating,
            "an actively-serving export must never expire mid-transfer (#216 Gap 1)",
        );
    }
    // The move lands; the dest owns the session.
    host.migration_commit(0);
    assert!(!host.sandboxes[0].migrating);
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
}

/// Never guess about ownership: an unreachable coordinator leaves the
/// expired export PAUSED (retry next sweep); the heal aborts it in place.
#[tokio::test(start_paused = true)]
async fn unreachable_coordinator_stays_paused_never_guesses() {
    let mut host = scenario_host(0, 2).await;
    host.guest_write(0, 5).await.unwrap();
    assert!(host.migration_begin(0).await.unwrap());

    tokio::time::advance(std::time::Duration::from_secs(150)).await;
    host.coord
        .script(ScriptedResponse::Unreachable("sim: coord down"));
    host.migration_ttl_sweep().await.unwrap();
    assert!(
        host.sandboxes[0].migrating,
        "unreachable coordinator ⇒ stay paused, never guess",
    );

    // Healed: the honest answer (we still own it) aborts in place.
    host.migration_ttl_sweep().await.unwrap();
    assert!(!host.sandboxes[0].migrating, "post-heal abort-in-place");
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
    host.guest_read(0, 5).await.unwrap();
}

/// #216 Gap 3 as the pure decision table: a REATTACHED frozen post-copy
/// source never destroys on a transient not-yet-rehydrated binding or an
/// unreachable coordinator — only an explicit `owned == false` does.
#[test]
fn reattached_source_verdict_never_destroys_on_a_transient_binding() {
    use engram_host_agent::migration::{reattach_source_verdict, ReattachSourceVerdict};
    // The binding hasn't rehydrated: NOT "nobody owns this" — stay paused
    // regardless of what the (unaskable) ownership answer would be.
    assert_eq!(
        reattach_source_verdict(false, None),
        ReattachSourceVerdict::StayPaused
    );
    assert_eq!(
        reattach_source_verdict(false, Some(false)),
        ReattachSourceVerdict::StayPaused,
        "an ownership answer is meaningless before the binding rehydrates",
    );
    // Bound: unreachable or still-owned ⇒ stay paused; only an explicit
    // `false` destroys.
    assert_eq!(
        reattach_source_verdict(true, None),
        ReattachSourceVerdict::StayPaused
    );
    assert_eq!(
        reattach_source_verdict(true, Some(true)),
        ReattachSourceVerdict::StayPaused
    );
    assert_eq!(
        reattach_source_verdict(true, Some(false)),
        ReattachSourceVerdict::Destroy
    );
}

/// The P9 lane's first CI catch (calm seeds 14/16): the sim modeled the
/// EXPLICIT coordinator abort as forbidden after state.bin shipped —
/// STRICTER than production, whose abort RPC documents it as legal (the
/// coordinator carries the postcopy-never-loaded knowledge, ADR 0045 C2).
/// The forbidden arm is only the dumb-host TTL self-resume, which
/// `ttl_verdict` gates in the sweep. This pins the legality: an explicit
/// abort on a served export resumes the source with every acked write
/// intact and NO oracle fires.
#[tokio::test(start_paused = true)]
async fn explicit_abort_after_state_served_is_legal_and_resumes() {
    let mut host = scenario_host(0, 2).await;
    host.guest_write(0, 6).await.unwrap();
    assert!(host.migration_begin(0).await.unwrap());
    host.migration_serve_state(0);

    // The coordinator learns the dest never loaded the shipped state and
    // explicitly aborts — legal despite state_served.
    host.migration_abort(0);
    assert!(!host.sandboxes[0].migrating, "the source resumes in place");
    assert!(
        host.split_brain_unpauses.is_empty(),
        "an explicit abort is not the TTL self-resume — no split-brain record",
    );
    invariants::check(&host)
        .await
        .unwrap_or_else(|v| panic!("{} — {}", v.invariant, v.detail));
    host.guest_read(0, 6).await.unwrap();
    // Live again: a fresh write acks.
    let before = host.ledger.len();
    host.guest_write(0, 6).await.unwrap();
    assert_eq!(host.ledger.len(), before + 1);
}

// ───────────── R5: storage lies — seeded read corruption (Phase 3) ─────────
//
// The seam (`CrashFs::with_read_fault`) lies about the bytes a stored file
// returns; `corrupt_spool_recovery` drives a spool rehydrate through it after
// raising a redundant published floor. Whatever the lie — a bit-flip on the
// meta marker or the first chunk — the recovery must DETECT it (the chunk
// re-hash, or R5's meta content-hash envelope) or TOLERATE it (rebuild from
// the floor), never a silent corrupt adopt. The oracle holds at every landing.

/// The end-to-end swarm step, exhaustively over the meta marker AND the first
/// chunk, at every byte offset the flip can land on. Each landing rehydrates
/// through the lie and must leave the acked-write oracle clean.
#[tokio::test(start_paused = true)]
async fn corrupt_spool_recovery_never_silently_adopts_and_the_oracle_holds() {
    for meta in [true, false] {
        for offset in 0..48usize {
            let mut host = scenario_host(0xB17E_0000 + offset as u64, 2).await;
            host.corrupt_spool_recovery(1, meta, offset)
                .await
                .unwrap_or_else(|e| {
                    panic!("corrupt_spool_recovery(meta={meta}, off={offset}): {e}")
                });
            invariants::check(&host).await.unwrap_or_else(|v| {
                panic!("meta={meta} off={offset}: {} — {}", v.invariant, v.detail)
            });
            // The redundant floor always survives; sandbox 1 chunk 0 reads its
            // flushed value (the un-published spooled write is a legitimate
            // transient-spool loss when the lie forced a discard).
            host.guest_read(1, 0).await.unwrap();
        }
    }
}

/// The sharp fail-without/pass-with pin at the recovery seam: a spool META
/// bit-rot that stays a SYNTACTICALLY VALID `SpoolMeta` (a bumped version — the
/// exact lie that defeats the rebuild's stale-spool lineage gate). R5's
/// envelope makes `read_spool` reject it as a loud `checksum` rollback; WITHOUT
/// the envelope the corrupt marker parses and is TRUSTED. This is the standing
/// spool the sim's rehydrate reads, so the format-level rejection is what keeps
/// `corrupt_spool_recovery` honest.
#[tokio::test(start_paused = true)]
async fn a_valid_but_corrupt_spool_marker_is_rejected_not_trusted() {
    let mut host = scenario_host(0xB17E_5EED, 2).await;
    // Establish a floor, then a standing spool holding a newer write.
    host.guest_write(0, 0).await.unwrap();
    host.flush_tick(0).await.unwrap();
    host.guest_write(0, 1).await.unwrap();
    host.spool_export(0).await.unwrap();
    let sid = host.sandboxes[0].sandbox_id;
    let meta_path = host.fs.spool_dir().join(sid.to_string()).join("meta.json");

    // Rewrite the sealed marker's BODY (bump the version) without fixing the
    // content hash — a perfect `SpoolMeta`, but a lie the envelope catches.
    let mut env: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&meta_path).await.unwrap()).unwrap();
    let mut meta: spool::SpoolMeta = serde_json::from_str(env["body"].as_str().unwrap()).unwrap();
    meta.version += 500;
    env["body"] = serde_json::Value::String(serde_json::to_string(&meta).unwrap());
    tokio::fs::write(&meta_path, serde_json::to_vec(&env).unwrap())
        .await
        .unwrap();

    let err = spool::read_spool(&TokioFs, host.fs.spool_dir(), sid)
        .await
        .expect_err("a bit-rotted-but-valid spool marker must be rejected, never trusted");
    assert!(
        err.to_string().contains("checksum"),
        "the R5 envelope names the content-hash gap: {err}",
    );
}
