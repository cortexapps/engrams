//! The 2026-08-02 durability rollback, at the coordinator↔host BOUNDARY —
//! the class test for the park-never-destroy quarantine ladder.
//!
//! # The incident shape
//!
//! A host-agent pod roll left survivors quarantined (disk unserved, no
//! `nbd_sandboxes` entry). The coordinator's ADR 0090 arm enqueued the keyed
//! quarantine evict; every capture attempt failed against the host's
//! refuse-untracked guard (correctly — a `disk_manifest=None` capture would
//! poison the lineage); and after `QUARANTINE_EVICT_MAX_ATTEMPTS = 3` the
//! exhaustion arm DESTROYED the VM — guaranteeing exactly the acked-write
//! loss the refusal existed to prevent. Eleven sessions rolled back between
//! 07-22 and 08-02.
//!
//! # The required behavior (this test)
//!
//! Exhaustion must PARK: the op stays queued on the slow retry lane (the
//! idempotency key keeps the 5s advertise deduped — no 8174b7aa-style op
//! flood), the session stays Active, the sandbox stays bound and alive, and
//! once the survivor's disk is re-served the parked op's next attempt
//! captures with zero rollback.

use engram_core::types::session::SessionState;
use engram_core::types::BindingDisposition;
use engram_dst_cosim::Cosim;

/// Build the incident's end-state: an ACTIVE session bound to a
/// record-invisible quarantined survivor (the gap-A recipe, without the
/// Created park — this is the flavor the 2026-08-02 sessions were in).
async fn wedge_active_session_over_quarantined_survivor(
    sim: &mut Cosim,
) -> (engram_core::SessionId, engram_core::SandboxId) {
    let session = sim.boot_session().await;
    let sandbox = sim.sandbox_of(session).await.expect("bound sandbox");
    sim.guest_work(session, 2).await;

    // HostLost is what makes the REAL coordinator rehydrate list omit the
    // survivor (the gap-A record-invisibility recipe); the guest itself
    // keeps RUNNING through the roll — the 2026-08-02 sessions were live,
    // un-parked VMs whose disk server died with the old pod.
    sim.force_session_state(session, SessionState::HostLost, BindingDisposition::Retain)
        .await;
    sim.roll_host().await;
    sim.lose_record(session).await;
    sim.register_rehydrate(true).await;
    assert!(
        sim.quarantined_unknown().await.contains(&sandbox),
        "precondition: the roll left a record-invisible quarantined survivor",
    );

    // The 2026-08-02 shape: the session is ACTIVE over the crippled VM (in
    // prod the session never left Active through the pod roll — only its
    // disk server died; the HostLost hop above is this recipe's device for
    // hiding the survivor from the rehydrate list). `transition_session`
    // rightly refuses HostLost → Active, so restore the prod end-state with
    // the raw db override.
    sim.world.meta.with_db_mut(|db| {
        db.sessions
            .get_mut(&session)
            .expect("session row exists")
            .session
            .status = SessionState::Active;
    });
    (session, sandbox)
}

/// The incident, replayed against the fixed ladder: advertise + drive until
/// well past the fast-retry budget. The op must PARK — session Active,
/// sandbox alive and still quarantined (never destroyed), op count
/// quiescent — instead of the old destroy → HostLost → rewind. Then the
/// recovery leg: re-serving the disk lets the parked op's next attempt
/// capture cleanly.
#[tokio::test(start_paused = true)]
async fn quarantine_exhaustion_parks_and_recovers_after_reserve() {
    let mut sim = Cosim::new(0x0802_0001).await;
    let (session, sandbox) = wedge_active_session_over_quarantined_survivor(&mut sim).await;

    // The prod loop: advertise every 5s, op executor drives in between.
    // 12 ticks × 5s = 60s — far past the 3-attempt fast budget at the
    // executor's capped backoff.
    let mut op_counts = Vec::new();
    for _ in 0..12 {
        sim.advertise_quarantined().await;
        sim.drive_ops().await;
        sim.advance(5).await;
        op_counts.push(sim.session_op_count(session));
    }

    // The survivor is PRESERVED: never destroyed, still bound, still
    // advertised as quarantined (the fuel is intentionally NOT burnt — the
    // op key, not a destroy, is what stops the flood).
    assert!(
        sim.quarantined_unknown().await.contains(&sandbox),
        "the crippled VM must still exist — exhaustion parks, never destroys",
    );
    assert_eq!(
        sim.sandbox_of(session).await,
        Some(sandbox),
        "the session keeps its binding to the survivor",
    );
    assert_eq!(
        sim.session_state(session).await,
        Some(SessionState::Active),
        "no HostLost settle — HostLost IS the rollback (resume would rewind \
         past the survivor's acked writes)",
    );

    // Op quiescence (the 8174b7aa class oracle): the parked op stays QUEUED
    // on its slow `not_before`, so its idempotency key dedupes every later
    // advertise — the count must stop growing even though the host still
    // advertises every tick.
    let settled = op_counts.last().copied().unwrap();
    assert_eq!(
        op_counts.first().copied().unwrap(),
        settled,
        "session_ops must quiesce while the op is parked, got {op_counts:?}",
    );

    // Recovery: the disk is re-served (prod: the host's rehydrate retry
    // pass; here the record heal + register pass the retry drives). The
    // parked op's next slow-lane attempt must then capture cleanly.
    sim.reconcile_quarantined().await;
    sim.register_rehydrate(true).await;
    assert!(
        sim.served_by_current(session).await,
        "the re-serve leg must bring the survivor's disk back",
    );

    // Wake the parked op (slow lane is 120s) and drive it.
    sim.advance(180).await;
    sim.advertise_quarantined().await;
    sim.drive_ops().await;
    assert_eq!(
        sim.session_state(session).await,
        Some(SessionState::Evicting),
        "with the disk re-served, the parked op's next attempt begins the \
         capture (evict_local → resume, losslessly) instead of failing",
    );
    sim.assert_slot_accounting().await.unwrap();
}
