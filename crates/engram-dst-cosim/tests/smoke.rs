//! Deliverable 1 (ADR 0098 R-CoSim, rung 1): the plumbing smoke.
//!
//! Create → serve → idle-evict → resume, entirely across the real
//! coordinator drivers and the real host-agent flows over one shared clock.
//! Proves the boundary bridge is wired both directions before the #570
//! reproduction leans on it.

use engram_core::types::session::SessionState;
use engram_dst_cosim::host::FINALIZE_MAX_ATTEMPTS;
use engram_dst_cosim::Cosim;

#[tokio::test(start_paused = true)]
async fn create_serve_evict_resume_roundtrip() {
    let mut sim = Cosim::new(0xC051_0001).await;

    // Create + boot to Active (real create_boot verb over the bridge).
    let session = sim.boot_session().await;
    assert_eq!(
        sim.session_state(session).await,
        Some(SessionState::Active),
        "boot reaches Active; trace={:?}",
        sim.trace
    );
    let sandbox = sim
        .sandbox_of(session)
        .await
        .expect("Active session is bound to a sandbox");

    // Some guest work + a periodic checkpoint, then more work — so the
    // eviction capture is strictly ahead of the last checkpoint.
    sim.guest_work(session, 3).await;
    sim.periodic_checkpoint(session).await;
    sim.guest_work(session, 2).await;

    // Go idle and evict via the real D5 fast path.
    sim.advance(3600).await;
    sim.evict_to_idle(session).await;
    assert_eq!(
        sim.session_state(session).await,
        Some(SessionState::Idle),
        "eviction reaches Idle; trace={:?}",
        sim.trace
    );
    // D5 cleared the coordinator's binding.
    assert_eq!(
        sim.sandbox_of(session).await,
        None,
        "D5 clears sessions.sandbox_id"
    );

    // The host-owned finalize completes → the durable recoverable snapshot
    // lands (prod: via the heartbeat reconcile).
    for _ in 0..=FINALIZE_MAX_ATTEMPTS {
        sim.finalize_pending().await;
    }
    sim.assert_idle_snapshot_durable(session)
        .unwrap_or_else(|e| panic!("{e}\ntrace={:?}", sim.trace));

    // The eviction finalize destroyed the paused VM at its terminal.
    assert!(
        sim.world
            .host
            .lock()
            .await
            .terminal_destroys()
            .contains(&sandbox),
        "the finalize terminal destroyed the captured sandbox"
    );

    // Resume the Idle session (real Resume verb over the bridge). Rung 1
    // asserts the resume op executes and ascends the session OUT of Idle
    // (it routes through the placement queue on the way back to Active); the
    // full re-placement-to-Active path is the same one `boot_session`
    // already exercises end to end.
    sim.resume_session(session).await;
    assert_ne!(
        sim.session_state(session).await,
        Some(SessionState::Idle),
        "resume ascends the session out of Idle; trace={:?}",
        sim.trace
    );
}
