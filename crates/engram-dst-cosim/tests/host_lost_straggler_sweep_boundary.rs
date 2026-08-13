//! Rung 2 (ADR 0098 R-CoSim, #784): the coordinator's REAL
//! `host_lost_straggler_sweep` driven ACROSS the boundary against the
//! REAL host-agent's `probe_sandbox`.
//!
//! ADR 0116 A-D5 (retiring the #777 serving-strike defer): a bound
//! `HostLost` row whose VM is still ALIVE past the 60s min-age is
//! entombed — the sweep records a tombstone, settles the ROW
//! immediately, and never destroys the serving VM (the tombstone rides
//! the heartbeat and the VM's own host destroys it). A VM the probe
//! reports gone gets the inline belt destroy instead. This is the exact
//! boundary interaction rung 1 could not co-simulate (its host had no
//! device plane and the sweep was never driven).

use engram_core::types::session::SessionState;
use engram_core::types::BindingDisposition;
use engram_dst_cosim::Cosim;

/// A bound HostLost row whose VM is ALIVE across the boundary: the
/// FIRST sweep settles the row and records a tombstone; the serving VM
/// is never destroyed by the coordinator, on this tick or any later
/// one.
#[tokio::test(start_paused = true)]
async fn hostlost_bound_alive_vm_is_entombed_and_settled_without_destroy() {
    let mut sim = Cosim::new(0x0777_0001).await;
    let session = sim.boot_session().await;
    let sandbox = sim.sandbox_of(session).await.expect("bound sandbox");
    let host = sim.world.host_id;

    // Dead-host detector flips the survivor to HostLost; the VM survives
    // (pidfd-reattached). The row keeps sandbox_id + host_id.
    sim.force_session_state(session, SessionState::HostLost, BindingDisposition::Retain)
        .await;
    // Age the row well past the sweep's 60s min-age.
    sim.advance(120).await;

    // The FIRST tick settles the row (no strike cap, no defer) and
    // entombs the VM.
    sim.straggler_sweep_tick().await;
    assert_ne!(
        sim.session_state(session).await,
        Some(SessionState::HostLost),
        "the row settles on the first sweep — it no longer waits on the VM"
    );
    assert_eq!(
        sim.sandbox_of(session).await,
        None,
        "the binding is cleared at settle"
    );
    assert!(
        sim.world.host.lock().await.contains(sandbox),
        "the SERVING VM is never destroyed by the coordinator (its tombstone owns it)"
    );
    assert_eq!(
        sim.world
            .state
            .services
            .meta
            .sandbox_tombstones_for_host(host)
            .await
            .unwrap(),
        vec![sandbox],
        "the tombstone carries the destroy obligation to the host"
    );

    // Later sweeps change nothing for the VM.
    sim.straggler_sweep_tick().await;
    assert!(sim.world.host.lock().await.contains(sandbox));
}

/// A VM the probe reports GONE (process not alive) gets the inline belt
/// destroy and the row settles the same tick — the tombstone still
/// records the obligation durably (ack-by-absence clears it once the
/// heartbeat confirms).
#[tokio::test(start_paused = true)]
async fn hostlost_bound_vm_gone_settles_immediately() {
    let mut sim = Cosim::new(0x0777_0002).await;
    let session = sim.boot_session().await;
    let sandbox = sim.sandbox_of(session).await.expect("bound sandbox");

    sim.force_session_state(session, SessionState::HostLost, BindingDisposition::Retain)
        .await;
    sim.advance(120).await;

    // The VM genuinely departs before the sweep runs (host destroyed it
    // out of band).
    sim.destroy_sandbox(sandbox).await;

    // The sweep settles immediately — the probe fails, the belt destroy
    // is a no-op, the row converges.
    sim.straggler_sweep_tick().await;
    assert_ne!(
        sim.session_state(session).await,
        Some(SessionState::HostLost),
        "a departed VM's HostLost row settles immediately"
    );
    assert_eq!(
        sim.sandbox_of(session).await,
        None,
        "binding cleared at settle"
    );
}
