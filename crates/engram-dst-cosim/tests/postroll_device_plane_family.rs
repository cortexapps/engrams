//! Rung 2 (ADR 0098 R-CoSim, #784): the NBD device-plane family co-simulated
//! at the coordinator↔host BOUNDARY.
//!
//! Rung 1 covered only the ownership leg of the post-roll survivor family
//! (`postroll_rehydrate_vs_sweep_vs_unpause.rs`). Rung 2 ports the REAL device
//! plane — generation / `NbdSlotAllocator` / `served_by` / park / the un-pause
//! gate / `guest_holds_device` (#806) — via
//! `engram_dst_host::device_plane::DevicePlane`, and drives the full family:
//! **host-agent roll → register-rehydrate (against the REAL coordinator
//! listing) → stale-binding sweep (REAL `sweep_verdict` incl. the #806 holder
//! table) → un-pause gate**, all real code on both sides.
//!
//! Every decision runs over the REAL host-core verdicts and the REAL slot
//! allocator; only the world bookkeeping is sim.

use engram_core::types::session::SessionState;
use engram_core::types::BindingDisposition;
use engram_dst_cosim::Cosim;

/// The FIXED happy path: a rung-parked survivor the coordinator LISTS (an
/// `Active`/`Evicting` reserves-host-memory row) is re-served by the coord-list
/// pass after a roll, the sweep skips the re-served device, and the un-pause
/// lands on a live plane.
#[tokio::test(start_paused = true)]
async fn roll_rehydrate_reserves_the_parked_survivor_then_unpause_succeeds() {
    let mut sim = Cosim::new(0x0784_0001).await;
    let session = sim.boot_session().await;
    sim.guest_work(session, 2).await;

    // Rung-2 park (FC paused, VM resident, device still served) — the 731df805
    // pre-condition — then the host-agent rolls (new generation).
    sim.park(session).await;
    assert!(sim.is_parked(session).await, "session is parked");
    sim.roll_host().await;
    assert!(
        !sim.served_by_current(session).await,
        "post-roll the device is not served by the new generation yet"
    );

    // Register-rehydrate against the REAL coordinator listing. The Active
    // session is reserves-host-memory ⇒ LISTED ⇒ re-served by the coord-list
    // pass; the sweep then skips the (claimed) device.
    sim.register_rehydrate(true).await;
    assert!(
        sim.served_by_current(session).await,
        "the listed survivor's device is re-served on the new generation"
    );

    // The un-pause data-plane gate passes (device served by this generation).
    assert!(
        sim.unpause(session).await,
        "un-pause lands on the live, re-served plane"
    );
    assert!(!sim.is_parked(session).await, "session un-paused");

    sim.assert_no_severed_live_holder().await.unwrap();
    sim.assert_slot_accounting().await.unwrap();
    sim.assert_ownership_agreement().await.unwrap();
}

/// The #806 guard at the boundary: a survivor the rehydrate passes MISS (a
/// `HostLost` row is not reserves-host-memory, so the REAL coordinator list
/// omits it; the #739 local pass is off — the ungated variant) reaches the
/// stale-binding sweep with its guest still holding the device open. The sweep
/// must PARK it (left kernel-bound, RECONNECTABLE), never DISCONNECT a live
/// guest's plane — and a later rehydrate re-serves it with zero device loss.
#[tokio::test(start_paused = true)]
async fn ungated_sweep_parks_the_live_held_survivor_then_reserves_zero_loss() {
    let mut sim = Cosim::new(0x0784_0002).await;
    let session = sim.boot_session().await;
    let sandbox = sim.sandbox_of(session).await.expect("bound sandbox");
    sim.guest_work(session, 3).await;

    sim.park(session).await;
    // The dead-host detector flips the survivor to HostLost — NOT
    // reserves-host-memory, so the register-rehydrate list omits it (the exact
    // "survivor invisible to the coord list" 731df805/#769 shape). The VM
    // stays resident; its guest keeps holding the rootfs device open.
    sim.force_session_state(session, SessionState::HostLost, BindingDisposition::Retain)
        .await;
    sim.roll_host().await;

    // Ungated rehydrate: coord list misses it AND the #739 local pass is off.
    sim.register_rehydrate(false).await;
    assert!(
        !sim.served_by_current(session).await,
        "the missed survivor is NOT re-served (coord list omits it, local pass off)"
    );
    // The sweep saw a dead-owner device a live guest still holds ⇒ PARK, never
    // Disconnect. The device stays kernel-bound (reconnectable).
    assert!(
        sim.sweep_parked_live().await.contains(&sandbox),
        "the sweep PARKed the live-held survivor's device (#806), never severed it"
    );
    sim.assert_no_severed_live_holder().await.unwrap();

    // Recovery: restore the survivor to a resident state so the coord list
    // names it again, then rehydrate re-serves it with zero device loss.
    sim.force_session_state(session, SessionState::Active, BindingDisposition::Retain)
        .await;
    sim.register_rehydrate(true).await;
    assert!(
        sim.served_by_current(session).await,
        "the parked-then-reconnectable device is re-served with zero loss"
    );
    sim.assert_no_severed_live_holder().await.unwrap();
    sim.assert_slot_accounting().await.unwrap();
}

/// The un-pause gate's OWN coverage: a genuinely-gone guest (FC crashed) leaves
/// no live holder, so the stale-binding sweep legally DISCONNECTs the device.
/// A later un-pause then finds an unserved plane and the gate FIRES — the guest
/// stays parked, routed to `evict_local → resume`, never landing on a dead
/// data plane (the last line of the 731df805 defense).
#[tokio::test(start_paused = true)]
async fn unpause_gate_fires_on_a_disconnected_dead_guest_plane() {
    let mut sim = Cosim::new(0x0784_0003).await;
    let session = sim.boot_session().await;
    let sandbox = sim.sandbox_of(session).await.expect("bound sandbox");

    sim.park(session).await;
    sim.force_session_state(session, SessionState::HostLost, BindingDisposition::Retain)
        .await;
    sim.roll_host().await;
    // The FC guest genuinely dies — it no longer holds the device node open.
    sim.kill_guest(session).await;

    // Ungated sweep: dead owner + NO live holder ⇒ proof of death ⇒ DISCONNECT.
    sim.register_rehydrate(false).await;
    assert!(
        !sim.sweep_parked_live().await.contains(&sandbox),
        "a genuinely-gone guest's device is legally disconnected, not parked"
    );
    // Disconnecting a device whose guest is gone is not a severing.
    sim.assert_no_severed_live_holder().await.unwrap();

    // The un-pause gate is the last line: the plane is unserved, so it fires.
    assert!(
        !sim.unpause(session).await,
        "the un-pause gate fires on the unserved plane — never a dead-plane serve"
    );
    assert!(
        sim.is_parked(session).await,
        "the guest stays parked, routed to recovery"
    );
    sim.assert_slot_accounting().await.unwrap();
}

/// Wave 7b (ADR 0098 §Phase 3, #784 layers 2–3 / #769 gap A) at the boundary:
/// the startup CLASSIFICATION BARRIER. A survivor whose tracked records are LOST
/// upstream (dropped from the coordinator listing AND its `ChainHeadRecord`
/// gone) rolls with its FC guest still resident and holding the device. NEITHER
/// rehydrate pass can re-serve it. The kernel-derived inventory reconciled
/// against the (missing) records QUARANTINES the device — classified + alertable
/// (`rehydrate-unknown-device`), left RECONNECTABLE — never skipped, never
/// severed. The operator/runbook reconcile then re-serves it with zero loss.
///
/// Distinguishes Wave 7b from #806: the raw R6 sweep PARKs a live holder but
/// records nothing at the record level. Here the barrier CLASSIFIES it into
/// `quarantined_unknown`. Fail-without: reverting the barrier leaves that set
/// empty and the assertion below fails.
#[tokio::test(start_paused = true)]
async fn gap_a_record_invisible_survivor_is_quarantined_then_recovers_zero_loss() {
    let mut sim = Cosim::new(0x0784_000A).await;
    let session = sim.boot_session().await;
    let sandbox = sim.sandbox_of(session).await.expect("bound sandbox");
    sim.guest_work(session, 2).await;

    sim.park(session).await;
    // The dead-host detector flips it to HostLost — NOT reserves-host-memory, so
    // the REAL coordinator listing OMITS it (the "survivor invisible to the coord
    // list" shape). Its LOCAL ChainHeadRecord is then LOST too (#769 gap A), so
    // the #739 local pass can't re-serve it either. The guest stays resident.
    sim.force_session_state(session, SessionState::HostLost, BindingDisposition::Retain)
        .await;
    sim.roll_host().await;
    sim.lose_record(session).await;

    // Both passes ON, but a survivor the coord list omits AND whose local record
    // is gone is re-servable by neither. The barrier must QUARANTINE it.
    sim.register_rehydrate(true).await;
    assert!(
        !sim.served_by_current(session).await,
        "no rehydrate pass could re-serve the record-invisible survivor",
    );
    // The barrier CLASSIFIED it (not silently skipped) and left it reconnectable.
    assert!(
        sim.quarantined_unknown().await.contains(&sandbox),
        "the barrier classified the record-invisible survivor as QuarantinedUnknown \
         (fail-without: the raw R6 sweep records nothing here)",
    );
    sim.assert_no_severed_live_holder().await.unwrap();
    sim.assert_quarantine_reconnectable().await.unwrap();

    // The operator/runbook reconcile the alert drives: restore the record; the
    // next rehydrate re-serves the reconnectable device with zero loss.
    sim.reconcile_quarantined().await;
    sim.register_rehydrate(true).await;
    assert!(
        sim.served_by_current(session).await,
        "the reconciled record let the reattach re-serve the quarantined device",
    );
    assert!(
        sim.unpause(session).await,
        "un-pause lands on the live re-served plane"
    );
    sim.assert_no_severed_live_holder().await.unwrap();
    sim.assert_slot_accounting().await.unwrap();
}
