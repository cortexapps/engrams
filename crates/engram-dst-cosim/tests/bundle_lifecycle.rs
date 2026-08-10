//! ADR 0035 amendment (2026-08-10 `chain_poisoned`): the aux-bundle
//! lifecycle at the coordinator↔host boundary.
//!
//! The incident's mechanism: a sandbox attaches the host's CURRENT stamp
//! generation at create; a bake roll rotates the stamp; the host sweep
//! deletes the node's only copy (the pre-amendment pin set was blind to
//! running-sandbox attachments); the generation was never published
//! (pre-amendment publish ran only at first snapshot reference); the next
//! capture's bundle-publish leg fails after the point of no return.
//!
//! Two directed pins:
//! * the PRE-fix world replayed with the red knobs
//!   (`roll_bundle_stamp(publish=false)` + a bare-empty-ack sweep) makes
//!   the reachability oracle FIRE and the next checkpoint FAIL — proving
//!   the oracle detects this class ahead of production;
//! * the POST-fix lifecycle (startup publish + heartbeat-fed pin set)
//!   upholds the oracle through stamp rolls, zero-grace GC sweeps, an
//!   eviction finalize, and a resume.

use engram_core::types::sandbox::AuxRoDrive;
use engram_core::types::session::SessionState;
use engram_dst_cosim::Cosim;

/// The 2026-08-10 interleaving, replayed with BOTH amendment halves
/// disabled: D1 off (`publish=false` — the generation never becomes
/// durable) and D2 off (the sweep runs against the pre-amendment ack — a
/// pin set with no live-sandbox leg, here the empty set a snapshotless,
/// catalog-less coordinator would send). The oracle must fire, and the
/// sandbox's next periodic checkpoint must fail its publish leg (the
/// production symptom: `bundle publish: open staged … No such file` after
/// FC consumed the dirty bitmap → chain poisoned).
#[tokio::test(start_paused = true)]
async fn incident_2026_08_10_pre_fix_replay_fires_the_oracle() {
    let mut sim = Cosim::new(0xB0DD_0001).await;

    // Pre-D1 world: the bake that is CURRENT when the session boots was
    // staged but never published (publish-on-first-snapshot semantics).
    sim.roll_bundle_stamp(false).await;
    let session = sim.boot_session().await;
    assert_eq!(sim.session_state(session).await, Some(SessionState::Active));
    let attached = {
        let host = sim.world.host.lock().await;
        host.sandbox_bundles()
    };
    assert_eq!(attached.len(), 1, "the sandbox attached the current stamp");
    let stranded_sha = attached[0].bundles[0].sha256.clone();

    // Healthy so far: the generation is staged locally (oracle holds).
    sim.assert_bundle_reachability()
        .await
        .expect("staged-locally is reachable");

    // The deploy: a NEW bake rotates the stamp; the old generation leaves
    // `current.json`.
    sim.roll_bundle_stamp(false).await;

    // The pre-D2 heartbeat ack: no snapshot pins the generation and the
    // pin set has no live-sandbox leg — the supervisor sweeps the node's
    // only copy out from under the running VM.
    sim.world
        .host
        .lock()
        .await
        .bundle_sweep(&[])
        .await
        .expect("sweep of an empty pin set");

    // The oracle fires: the attached generation is staged nowhere and
    // absent from blob storage.
    let err = sim
        .assert_bundle_reachability()
        .await
        .expect_err("the stranded attachment must fire the oracle");
    assert!(
        err.contains("chain_poisoned class"),
        "oracle names the incident class: {err}"
    );
    assert!(err.contains(&stranded_sha), "oracle names the sha: {err}");

    // The production symptom: the next periodic checkpoint cannot make its
    // pinned generation durable — no recoverable row lands.
    sim.guest_work(session, 2).await;
    sim.periodic_checkpoint(session).await;
    assert_eq!(
        sim.newest_recoverable_cursor(session),
        None,
        "the checkpoint's publish leg failed; no snapshot row may land \
         (recording one would be a pin nothing can satisfy); trace={:?}",
        sim.trace
    );
}

/// The FIXED lifecycle upholds reachability end to end: startup publish
/// makes every staged generation durable at birth; the heartbeat-fed pin
/// set (live-sandbox attachments + stamps) protects it from the host
/// sweep and a ZERO-grace bundle GC; the eviction finalize records the
/// pin onto the snapshot row so durability outlives the sandbox; a
/// resume attaches the new current generation. The oracle holds at every
/// step.
#[tokio::test(start_paused = true)]
async fn post_fix_lifecycle_upholds_reachability() {
    let mut sim = Cosim::new(0xB0DD_0002).await;

    let session = sim.boot_session().await;
    assert_eq!(sim.session_state(session).await, Some(SessionState::Active));
    let gen1_sha = {
        let host = sim.world.host.lock().await;
        host.sandbox_bundles()[0].bundles[0].sha256.clone()
    };

    // The deploy: a new bake rolls the stamp (WITH the D1 startup
    // publish), then the heartbeat cycle runs the pin-set sweep, then the
    // adversarial zero-grace GC marks + promotes.
    sim.roll_bundle_stamp(true).await;
    sim.host_heartbeat().await;
    sim.bundle_gc_sweep().await;
    sim.bundle_gc_sweep().await;
    sim.assert_bundle_reachability()
        .await
        .expect("a running sandbox's generation survives roll + sweep + GC");
    assert!(
        sim.world
            .blob
            .exists(&AuxRoDrive::blob_key(&gen1_sha))
            .await
            .unwrap(),
        "the attached generation stays durable under zero-grace GC (the \
         sandbox_bundles pin leg)"
    );

    // Evict: the finalize's REAL bundle-publish leg runs (HEAD hit — D1
    // already published) and the snapshot row records the pin.
    sim.guest_work(session, 3).await;
    sim.advance(3600).await;
    sim.evict_to_idle(session).await;
    sim.finalize_pending().await;
    assert_eq!(
        sim.session_state(session).await,
        Some(SessionState::Idle),
        "finalize completed; trace={:?}",
        sim.trace
    );
    sim.assert_bundle_reachability()
        .await
        .expect("the recorded snapshot's pins are durable");

    // The sandbox is destroyed; the SNAPSHOT leg now carries the pin.
    // Heartbeat (its sandbox_bundles no longer report it) + GC again: the
    // generation must survive on the snapshot row's pin alone.
    sim.host_heartbeat().await;
    sim.bundle_gc_sweep().await;
    sim.bundle_gc_sweep().await;
    assert!(
        sim.world
            .blob
            .exists(&AuxRoDrive::blob_key(&gen1_sha))
            .await
            .unwrap(),
        "an evicted session's pinned generation survives GC on the \
         snapshot-row leg (its restore must be able to materialize it)"
    );

    // Resume (rung-1 contract, same as the smoke: the resume ascends the
    // session OUT of Idle — full re-placement to Active is the boot path's
    // territory). Whatever the resume re-created attaches the NEW current
    // generation; the world stays reachable.
    sim.resume_session(session).await;
    assert_ne!(
        sim.session_state(session).await,
        Some(SessionState::Idle),
        "resume ascends the session out of Idle; trace={:?}",
        sim.trace
    );
    sim.assert_bundle_reachability()
        .await
        .expect("post-resume attachments are reachable");
}

/// The "never became durable" half under fault injection: the FIRST put
/// to the `bundles/` prefix fails (a GCS blip at host startup), so the
/// world boots with a staged-but-unpublished stamp. The oracle must HOLD
/// through the window (the stamp keep-set protects the local copy), and
/// the heartbeat's retry must land durability — the real startup task's
/// 60 s retry, driven here by the heartbeat step.
#[tokio::test(start_paused = true)]
async fn faulted_startup_publish_retries_on_heartbeat_and_stays_reachable() {
    use engram_testkit::storage::{FaultPlan, InjectedError};
    let plan = FaultPlan::new().fail_nth_put(1, InjectedError::Sdk("gcs blip".into()));
    let mut sim = Cosim::new_with_fault_plan(0xB0DD_0003, Some(plan)).await;

    let session = sim.boot_session().await;
    assert_eq!(sim.session_state(session).await, Some(SessionState::Active));
    let sha = {
        let host = sim.world.host.lock().await;
        assert!(
            !host.stamp_published(),
            "the faulted first put left the stamp un-durable"
        );
        host.sandbox_bundles()[0].bundles[0].sha256.clone()
    };
    assert!(
        !sim.world
            .blob
            .exists(&AuxRoDrive::blob_key(&sha))
            .await
            .unwrap(),
        "not yet durable"
    );
    // The window is safe: staged-locally satisfies reachability (the
    // sweep's stamp keep-set protects the file until publish lands).
    sim.assert_bundle_reachability()
        .await
        .expect("staged-but-unpublished window is reachable");

    // The heartbeat retries the startup publish (fault plan exhausted) and
    // the pin-set sweep runs; durability lands.
    sim.host_heartbeat().await;
    assert!(
        sim.world.host.lock().await.stamp_published(),
        "the heartbeat retry published the stamp"
    );
    assert!(
        sim.world
            .blob
            .exists(&AuxRoDrive::blob_key(&sha))
            .await
            .unwrap(),
        "durable after the retry"
    );
    sim.assert_bundle_reachability().await.expect("reachable");
}
