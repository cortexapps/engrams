//! Deliverable 4b (ADR 0098 R-CoSim, rung 1): the post-roll survivor family
//! (the 731df805 / #739 / #769 shape), at the coordinator↔host BOUNDARY.
//!
//! # Scope (documented divergence)
//!
//! The FULL post-roll family also spans the host's NBD generation / slot /
//! park / un-pause-gate data-plane model — which lives in `engram-dst-host`'s
//! `SimHost` and is NOT carried into this boundary host (rung 1 keys sandboxes
//! by the coordinator's id-space and reuses the finalize/reconcile flows, not
//! the NBD machinery). Porting that model into the boundary host is rung-2
//! work. What rung 1 CAN co-simulate — and does here — is the **ownership**
//! leg of the family: the ADR 0090 survivor-reattach interaction between the
//! coordinator's `session_owning_sandbox` (non-terminal, INCLUDING
//! `HostLost`) and the host's real `reconcile_once` Unbound arm. That is
//! exactly the class where a survivor invisible to a naive lookup gets
//! wrongly reaped.
//!
//! # The #777 tension, pinned
//!
//! A `HostLost` row whose VM is still alive is a survivor: the reconcile,
//! finding no local binding after the roll, asks the coordinator "who owns
//! this?" and — because `HostLost` is non-terminal — gets the session back,
//! so it REPAIRS the binding rather than reaping. This test pins that CURRENT
//! behavior. The open design call it records: the coordinator-side
//! `host_lost_straggler_sweep`'s 60s min-age eventually decides such a
//! survivor's fate; co-simulating the sweep vs. this repair (and the un-pause
//! gate) together is the rung-2 increment.

use engram_core::types::session::SessionState;
use engram_core::types::BindingDisposition;
use engram_dst_cosim::Cosim;

/// A `HostLost` survivor with a live VM but a dropped local binding must be
/// REPAIRED by reconcile, never reaped (ADR 0090; the 731df805/#739 class).
#[tokio::test(start_paused = true)]
async fn hostlost_survivor_with_dropped_binding_is_repaired_not_reaped() {
    let mut sim = Cosim::new(0x0769_0001).await;
    let session = sim.boot_session().await;
    let sandbox = sim
        .sandbox_of(session)
        .await
        .expect("Active session bound to a sandbox");

    // The dead-host detector flips the session to HostLost; the VM survives
    // (pidfd-reattached). The coordinator row keeps sandbox_id + host_id.
    sim.force_session_state(session, SessionState::HostLost, BindingDisposition::Retain)
        .await;
    // The host-agent rolled: its in-RAM binding table died, so reconcile has
    // no local binding for the surviving VM (the exact post-roll blind spot).
    sim.drop_local_binding(sandbox);

    // Drive the real reconcile past the strike threshold. The Unbound arm
    // asks `sandbox_owner`; the coordinator returns the HostLost session, so
    // the verdict is RepairBinding — never Orphan.
    for _ in 0..3 {
        sim.reconcile_tick(true).await;
    }

    assert!(
        !sim.reconcile_destroys().contains(&sandbox),
        "a HostLost survivor must NEVER be reaped (ADR 0090 / 731df805)"
    );
    assert!(
        sim.reconcile_binding_repairs()
            .iter()
            .any(|(sb, se)| *sb == sandbox && *se == session),
        "reconcile repaired the survivor's local binding from the coordinator"
    );
}

/// The contrast that proves the exemption is scoped: a TERMINAL session's
/// leftover VM (genuinely departed) is still reaped — the coordinator
/// confirms no owner (`session_owning_sandbox` excludes terminal states), so
/// the Unbound arm is a true Orphan.
#[tokio::test(start_paused = true)]
async fn terminal_sessions_leftover_vm_is_still_reaped() {
    let mut sim = Cosim::new(0x0769_0002).await;
    let session = sim.boot_session().await;
    let sandbox = sim
        .sandbox_of(session)
        .await
        .expect("Active session bound to a sandbox");

    // The session failed terminally, but its VM leaked and its local binding
    // is gone.
    sim.force_session_state(session, SessionState::Failed, BindingDisposition::Detach)
        .await;
    sim.drop_local_binding(sandbox);

    // ADR 0116 A5: ticks at the real cadence — the first stamps the
    // first-seen mark, and the age grace (2x the interval) clears with
    // the time advances between ticks.
    for _ in 0..3 {
        sim.reconcile_tick(true).await;
        sim.advance(120).await;
    }

    assert!(
        sim.reconcile_destroys().contains(&sandbox),
        "a terminal session's confirmed-orphan VM is reaped once the age grace clears"
    );
}
