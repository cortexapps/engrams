//! Rung 2 (ADR 0098 R-CoSim, #784, deliverable 5): the capture-lock release
//! pin.
//!
//! #783 gave the teardown-reconcile ownership check a `capture_in_flight`
//! exemption — a sandbox holding the eviction capture lock is NOT reaped, so
//! the mid-upload finalize can complete (the #570 fix). That exemption is only
//! safe if the capture lock is ALWAYS released when the finalize ends — on
//! completion AND on QUARANTINE (redrive budget exhausted). If a quarantined
//! finalize left the lock held, the exemption would become a PERMANENT
//! reap-shield: the sandbox could never be torn down, wedging the session.
//!
//! Rung 1's finalize-convergence oracle implies this only indirectly. This test
//! pins it directly against the REAL `run_eviction_finalize_attempt`: force a
//! finalize to quarantine (every redrive fails on a deleted staging dir) and
//! assert `capture_in_flight → false`.

use engram_dst_cosim::host::{FinalizeTickOutcome, FINALIZE_MAX_ATTEMPTS};
use engram_dst_cosim::Cosim;

#[tokio::test(start_paused = true)]
async fn quarantined_finalize_releases_the_capture_lock() {
    let mut sim = Cosim::new(0x0784_0005).await;
    let session = sim.boot_session().await;
    let sandbox = sim.sandbox_of(session).await.expect("bound sandbox");
    sim.guest_work(session, 2).await;

    // Begin the REAL eviction capture leg: the capture lock is taken.
    sim.begin_finalize(sandbox)
        .await
        .expect("snapshot_begin takes the capture lock");
    assert!(
        sim.capture_in_flight(sandbox).await,
        "the capture lock is held while the finalize is in flight (#783 exemption applies)"
    );

    // Sabotage the staging dir so every redrive attempt fails (the ENOENT
    // class), driving the REAL finalizer to quarantine after the budget.
    sim.sabotage_finalize_staging(sandbox).await;

    let mut quarantined = false;
    for _ in 0..(FINALIZE_MAX_ATTEMPTS + 2) {
        match sim.finalize_tick(sandbox).await {
            FinalizeTickOutcome::Quarantined { .. } => {
                quarantined = true;
                break;
            }
            FinalizeTickOutcome::Completed { .. } => {
                panic!("the sabotaged finalize must not complete");
            }
            FinalizeTickOutcome::Retrying | FinalizeTickOutcome::Idle => {
                sim.advance(600).await;
            }
        }
    }
    assert!(
        quarantined,
        "the finalize quarantines once the redrive budget is exhausted"
    );

    // THE PIN: quarantine released the capture lock, so the teardown-reconcile
    // exemption no longer applies — the sandbox is reapable, never a permanent
    // reap-shield.
    assert!(
        !sim.capture_in_flight(sandbox).await,
        "a QUARANTINED finalize releases the capture lock (capture_in_flight → false), so \
         #783's teardown-reconcile exemption can never become a permanent reap-shield"
    );
}
