//! The 2026-07-21 8174b7aa livelock, at the coordinator↔host BOUNDARY
//! (ADR 0090 × ADR 0077; the PR #824 prevention set's class test).
//!
//! # The incident shape
//!
//! A host roll leaves a survivor quarantined (record-invisible, disk
//! unserved). A resume's `start_agent` then fails against the crippled VM
//! and the ADR 0077 contract parks the session at `Created`. From there two
//! locally-correct mechanisms deadlock: the host advertises the quarantined
//! survivor every 5s heartbeat; the coordinator's ADR 0090 arm enqueues the
//! keyed quarantine `evict_local`; and (pre-#824) the evict guard skipped
//! `Created` as unevictable in ~10ms — freeing the queued/running-scoped
//! idempotency key before the next heartbeat. Enqueue → skip → re-enqueue,
//! forever: 2.5 days and ~43k spurious `session_ops` rows in prod.
//!
//! # Why this file exists
//!
//! The cosim previously wrote host liveness straight to the meta store, so
//! the advertise → enqueue seam — where the loop lived — was structurally
//! invisible to it. `Cosim::advertise_quarantined` now drives the REAL
//! `quarantined_survivor_advertise_core`, and the op-quiescence oracle
//! (`session_op_count` must stop growing under a fixed world state) catches
//! the whole enqueue → fast-skip → re-enqueue class, not just this bug —
//! the ADR 0093 423-row pileup is the same signature.

use engram_core::types::session::SessionState;
use engram_dst_cosim::Cosim;

/// Build the incident's end-state: a quarantined survivor (record-invisible
/// after a roll, per the gap-A recipe) whose session sits at `Created` (the
/// ADR 0077 harness-failed park), still bound to the crippled sandbox.
async fn wedge_created_park_over_quarantined_survivor(
    sim: &mut Cosim,
) -> (engram_core::SessionId, engram_core::SandboxId) {
    let session = sim.boot_session().await;
    let sandbox = sim.sandbox_of(session).await.expect("bound sandbox");
    sim.guest_work(session, 2).await;

    // The roll: the survivor is invisible to BOTH rehydrate passes (the
    // coord list omits HostLost — not reserves-host-memory — and the local
    // ChainHeadRecord is lost), so the barrier QUARANTINES it.
    sim.park(session).await;
    sim.force_session_state(session, SessionState::HostLost)
        .await;
    sim.roll_host().await;
    sim.lose_record(session).await;
    sim.register_rehydrate(true).await;
    assert!(
        sim.quarantined_unknown().await.contains(&sandbox),
        "precondition: the roll left a record-invisible quarantined survivor",
    );

    // The failed-resume park (prod: HostLost → recovery → resume whose
    // start_agent failed against the crippled VM → parked at Created, still
    // bound). `force_session_state` is the sanctioned shortcut for the
    // sequence's end-state.
    sim.force_session_state(session, SessionState::Created)
        .await;
    (session, sandbox)
}

/// The incident, replayed: heartbeat advertises + op drives must CONVERGE —
/// first driven op reaps the survivor (destroy clears the host's quarantine
/// entry, killing the advertise source) and settles `HostLost` — and the
/// op count must QUIESCE. Pre-#824 this loop grew one op per tick, forever.
#[tokio::test(start_paused = true)]
async fn quarantine_advertise_on_created_park_converges_and_ops_quiesce() {
    let mut sim = Cosim::new(0x0824_0001).await;
    let (session, sandbox) = wedge_created_park_over_quarantined_survivor(&mut sim).await;

    // The prod loop: advertise every 5s, op executor drives in between.
    let mut op_counts = Vec::new();
    let mut advertised = Vec::new();
    for _ in 0..6 {
        advertised.push(sim.advertise_quarantined().await);
        sim.drive_ops().await;
        sim.advance(5).await;
        op_counts.push(sim.session_op_count(session));
    }

    // Convergence: the FIRST driven op reaped the survivor — the destroy
    // removed the host's quarantine entry, so every later advertise had
    // nothing to report (the loop's fuel is gone, not merely unburnt).
    assert!(
        advertised[0] >= 1 && advertised[1..].iter().all(|&n| n == 0),
        "the advertise source must die with the first reap, got {advertised:?}",
    );
    assert!(
        sim.quarantined_unknown().await.is_empty(),
        "the reap cleared the quarantine entry",
    );
    assert_eq!(
        sim.session_state(session).await,
        Some(SessionState::HostLost),
        "the wedged Created park settles HostLost — the one lane the \
         dead-host straggler sweep re-drives to Idle/recoverable \
         (that recovery leg is pinned by host_lost_straggler_sweep_boundary)",
    );
    assert!(
        !sim.quarantined_unknown().await.contains(&sandbox),
        "the crippled VM is gone",
    );

    // THE CLASS ORACLE — op quiescence: after the first tick's single
    // convergence op, the count must never grow again. The pre-#824
    // signature is one fresh op per tick (op_counts strictly increasing);
    // prod ran that signature to ~43k rows.
    assert_eq!(
        op_counts.first(),
        op_counts.last(),
        "session_ops must quiesce under a fixed world state — growth here \
         is the enqueue → fast-skip → re-enqueue livelock, got {op_counts:?}",
    );
}

// NOTE deliberately NOT tested here: the ADR 0079 finding-#4 "re-adverts
// dedup against a still-queued op" property. Through the REAL core that
// state is unreachable in a directed harness — prod's `session_ops::enqueue`
// detaches an inline drive onto a spawn, so an enqueued op is claimed before
// the next advertise can observe it queued (in prod the pileup only forms
// when the executor lags the 5s heartbeat). The property is pinned where it
// IS constructible: `host_http`'s `quarantined_survivor_readverts_dedup_to_
// one_op`, which seeds a running evict to hold the lane.
