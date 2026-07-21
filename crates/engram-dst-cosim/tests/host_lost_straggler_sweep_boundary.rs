//! Rung 2 (ADR 0098 R-CoSim, #784): the coordinator's REAL
//! `host_lost_straggler_sweep` (#782/#777) driven ACROSS the boundary against
//! the REAL host-agent's `probe_sandbox`.
//!
//! The #777 tension: a bound `HostLost` row whose VM is still ALIVE past the
//! 60s min-age. The sweep's ask-the-host policy consults host truth
//! (`probe_sandbox → process_alive`) before destroying: a live-and-serving VM
//! under a HostLost row is the >60s partition/desync window the reattach
//! machinery may still recover, so the sweep DEFERS (banking a serving-strike)
//! rather than killing the live VM — until the strike cap, then it destroys +
//! settles (bounded convergence, no #762/#769 eternal wedge). This is the exact
//! boundary interaction rung 1 could not co-simulate (its host had no device
//! plane and the sweep was never driven).

use engram_core::types::session::SessionState;
use engram_dst_cosim::Cosim;

/// A bound HostLost row whose VM is ALIVE: the sweep DEFERS (ask-the-host,
/// #777) for `strike_cap` cycles, never killing the live VM prematurely, then
/// destroys + settles — bounded convergence.
#[tokio::test(start_paused = true)]
async fn hostlost_bound_alive_vm_defers_then_settles_at_strike_cap() {
    let mut sim = Cosim::new(0x0777_0001).await;
    let session = sim.boot_session().await;
    let sandbox = sim.sandbox_of(session).await.expect("bound sandbox");

    // Dead-host detector flips the survivor to HostLost; the VM survives
    // (pidfd-reattached). The row keeps sandbox_id + host_id.
    sim.force_session_state(session, SessionState::HostLost)
        .await;
    // Age the row well past the sweep's 60s min-age.
    sim.advance(120).await;

    // The default strike cap is 3 (`DeadHostConfig::straggler_serving_strike_cap`).
    // Ticks 1 and 2 DEFER (the VM probes alive-and-serving across the boundary).
    for tick in 1..=2 {
        sim.straggler_sweep_tick().await;
        assert_eq!(
            sim.session_state(session).await,
            Some(SessionState::HostLost),
            "tick {tick}: a live-serving survivor is DEFERRED, not settled"
        );
        assert_eq!(
            sim.sandbox_of(session).await,
            Some(sandbox),
            "tick {tick}: the live VM's binding is NOT torn down (ask-the-host defer)"
        );
    }

    // The 3rd tick reaches the strike cap: the sweep gives up and destroys +
    // settles (bounded convergence — the #762/#769 eternal-wedge is not
    // reintroduced).
    sim.straggler_sweep_tick().await;
    assert_ne!(
        sim.session_state(session).await,
        Some(SessionState::HostLost),
        "at the strike cap the straggler sweep settles the row (converges)"
    );
    assert_eq!(
        sim.sandbox_of(session).await,
        None,
        "the sandbox binding is cleared at settle"
    );
}

/// The ask-the-host path where the VM genuinely departs between ticks: after
/// one defer the VM is destroyed, so the next probe FAILS (process not alive)
/// and the sweep settles immediately — no wait for the strike cap.
#[tokio::test(start_paused = true)]
async fn hostlost_bound_vm_gone_settles_immediately_without_strikes() {
    let mut sim = Cosim::new(0x0777_0002).await;
    let session = sim.boot_session().await;
    let sandbox = sim.sandbox_of(session).await.expect("bound sandbox");

    sim.force_session_state(session, SessionState::HostLost)
        .await;
    sim.advance(120).await;

    // First tick: VM alive ⇒ defer.
    sim.straggler_sweep_tick().await;
    assert_eq!(
        sim.session_state(session).await,
        Some(SessionState::HostLost),
        "first tick defers the live VM"
    );

    // The VM genuinely departs (host destroys it out of band).
    sim.destroy_sandbox(sandbox).await;

    // Next tick: the probe fails (process not alive) ⇒ settle immediately.
    sim.straggler_sweep_tick().await;
    assert_ne!(
        sim.session_state(session).await,
        Some(SessionState::HostLost),
        "a departed VM's HostLost row settles immediately (no strike-cap wait)"
    );
    assert_eq!(
        sim.sandbox_of(session).await,
        None,
        "binding cleared at settle"
    );
}
