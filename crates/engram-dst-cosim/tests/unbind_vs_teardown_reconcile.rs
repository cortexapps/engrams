//! Deliverables 2 + 3 (ADR 0098 R-CoSim, rung 1): the #570 reproduction and
//! the minimal product fix, held to the pinned-seed red-then-green
//! discipline.
//!
//! # The #570 window (gh issue view 570)
//!
//! Idle eviction takes the ADR 0045 D5 fast path: the instant the host's
//! `snapshot_begin` returns (capture durably staged, capture lock held), the
//! coordinator marks the session `Idle` and clears `sessions.sandbox_id`,
//! and the snapshot upload finalizes in a host-owned background job. During
//! that window the host's teardown-reconcile sweep lists the still-resident
//! (paused) VM, asks the coordinator "does this session still own it?", gets
//! `false` (the D5 unbind cleared the binding), counts orphan strikes, and
//! **destroys the VM mid-upload** — cancelling the finalize. No snapshot row
//! lands; the next resume falls back to a stale periodic checkpoint and
//! rewinds completed work.
//!
//! Both legs run REAL code co-simulated: the coordinator's real D5 Evict
//! verb + `sandbox_ownership` handler core, and the host's real
//! `reconcile_once` + `EvictionFinalizer`. The only knob is
//! `honor_capture_signal` on the reconcile backend — the adversarial replay
//! of the pre-fix reconcile that never consulted the capture-in-flight
//! signal.

use engram_core::{SandboxId, SessionId};
use engram_dst_cosim::host::FINALIZE_MAX_ATTEMPTS;
use engram_dst_cosim::Cosim;

/// Drive a session to the mid-D5 window: booted, checkpointed at an earlier
/// cursor, more work, then idle-evicted to `Idle` with the finalize STILL in
/// flight (capture lock held, not yet completed). Returns `(session,
/// sandbox)`.
async fn into_mid_capture_window(sim: &mut Cosim) -> (SessionId, SandboxId) {
    let session = sim.boot_session().await;
    let sandbox = sim
        .sandbox_of(session)
        .await
        .expect("Active session bound to a sandbox");

    // Work to cursor 3, a periodic checkpoint there, then work to cursor 5 —
    // so a lost eviction snapshot rewinds to cursor 3 (the stale checkpoint).
    sim.guest_work(session, 3).await;
    sim.periodic_checkpoint(session).await;
    sim.guest_work(session, 2).await;

    sim.advance(3600).await;
    sim.evict_to_idle(session).await;

    // Precondition of the window: Idle, unbound, capture still in flight.
    assert_eq!(
        sim.session_state(session).await,
        Some(engram_core::types::session::SessionState::Idle),
        "D5 reached Idle"
    );
    assert_eq!(sim.sandbox_of(session).await, None, "D5 cleared sandbox_id");
    assert!(
        sim.capture_in_flight(sandbox).await,
        "the eviction finalize is still in flight (capture lock held) — the #570 window"
    );
    assert_eq!(
        sim.evict_cursor(session),
        Some(5),
        "capture taken at cursor 5"
    );
    (session, sandbox)
}

/// RED: the pre-fix reconcile (never consults `capture_in_flight`) reaps the
/// mid-capture VM, cancelling the finalize — no durable snapshot lands and
/// the oracle fires.
#[tokio::test(start_paused = true)]
async fn buggy_reconcile_reaps_mid_capture_and_loses_the_snapshot() {
    let mut sim = Cosim::new(0x570_0001).await;
    let (session, sandbox) = into_mid_capture_window(&mut sim).await;

    // Teardown-reconcile with the signal SUPPRESSED (pre-fix behavior). Two
    // ticks cross ORPHAN_STRIKES and reap the still-capturing VM.
    sim.reconcile_tick(false).await;
    sim.reconcile_tick(false).await;
    assert!(
        sim.reconcile_destroys().contains(&sandbox),
        "the pre-fix reconcile destroyed the mid-capture sandbox (issue #570)"
    );
    assert!(
        !sim.capture_in_flight(sandbox).await,
        "the destroy cancelled the in-flight finalize"
    );

    // The host-owned finalize can no longer complete (its inputs were
    // dropped). No snapshot row lands.
    for _ in 0..=FINALIZE_MAX_ATTEMPTS {
        sim.finalize_pending().await;
    }

    // The oracle FIRES: the newest recoverable snapshot is the stale
    // cursor-3 checkpoint, below the cursor-5 eviction.
    let verdict = sim.assert_idle_snapshot_durable(session);
    assert!(
        verdict.is_err(),
        "issue #570 must reproduce: expected a durability violation, but the oracle passed \
         (newest recoverable = {:?}, evicted at {:?})",
        sim.newest_recoverable_cursor(session),
        sim.evict_cursor(session),
    );
    assert_eq!(
        sim.newest_recoverable_cursor(session),
        Some(3),
        "resume would rewind to the stale periodic checkpoint at cursor 3"
    );
}

/// GREEN: with the fix (reconcile honors `capture_in_flight`), the
/// mid-capture VM is exempt, the finalize completes, the durable snapshot
/// lands at the eviction cursor, and the oracle holds.
#[tokio::test(start_paused = true)]
async fn fixed_reconcile_exempts_mid_capture_and_the_snapshot_is_durable() {
    let mut sim = Cosim::new(0x570_0001).await;
    let (session, sandbox) = into_mid_capture_window(&mut sim).await;

    // Same reconcile pressure, now with the capture signal HONORED (the fix).
    sim.reconcile_tick(true).await;
    sim.reconcile_tick(true).await;
    assert!(
        !sim.reconcile_destroys().contains(&sandbox),
        "the fixed reconcile exempts a mid-capture sandbox — never reaps it"
    );
    assert!(
        sim.capture_in_flight(sandbox).await,
        "the finalize is still in flight after reconcile — untouched"
    );

    // The host-owned finalize completes and lands the durable snapshot.
    for _ in 0..=FINALIZE_MAX_ATTEMPTS {
        sim.finalize_pending().await;
    }

    sim.assert_idle_snapshot_durable(session)
        .unwrap_or_else(|e| panic!("the fix must keep the eviction snapshot durable: {e}"));
    assert_eq!(
        sim.newest_recoverable_cursor(session),
        Some(5),
        "the eviction snapshot at cursor 5 is the resume's selection"
    );
}
