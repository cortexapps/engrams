//! Deliverable 4a (ADR 0098 R-CoSim, rung 1): the host publishes a live disk
//! manifest, the coordinator commits it, and the ACK is lost on the wire — so
//! the host retries. This pins the idempotency of the survivor-publish path
//! (the flush / eviction-finalize legs' `publish_live_manifest`), driven
//! through the REAL coordinator handler core (`live_manifest_publish_core`)
//! over the boundary bridge.
//!
//! The property: a lost ACK is safe. Re-publishing an already-committed
//! manifest is idempotent w.r.t. the durable survivor pointer (no rollback,
//! no divergence), and a publish from a DEPARTED binding (the sandbox was
//! unbound / rebound) is dropped as `Stale` rather than clobbering the
//! session's current pointer — the structural staleness the coordinator
//! logs and the host must not paper over with a blind retry.

use engram_core::types::manifest::ManifestRef;
use engram_dst_cosim::Cosim;
use engram_host_core::LiveManifestPublishOutcome;

#[tokio::test(start_paused = true)]
async fn lost_ack_retry_is_idempotent_and_a_stale_publish_is_dropped() {
    let mut sim = Cosim::new(0x0602_0001).await;
    let session = sim.boot_session().await;
    let sandbox = sim
        .sandbox_of(session)
        .await
        .expect("Active session bound to a sandbox");

    let m1 = uuid::Uuid::from_u128(0x1111);
    let m2 = uuid::Uuid::from_u128(0x2222);

    // First publish commits manifest v1.
    assert_eq!(
        sim.publish_manifest(session, sandbox, m1, 1).await,
        LiveManifestPublishOutcome::Applied,
        "the first publish commits"
    );
    assert_eq!(
        sim.live_disk_manifest(session),
        Some(ManifestRef {
            manifest_id: m1,
            version: 1
        }),
        "the durable survivor pointer now names v1"
    );

    // The ACK was lost — the host retries the SAME publish. It must be
    // idempotent: still Applied, the pointer unchanged (no rollback, no
    // divergence).
    assert_eq!(
        sim.publish_manifest(session, sandbox, m1, 1).await,
        LiveManifestPublishOutcome::Applied,
        "a lost-ACK retry of the same manifest re-applies cleanly"
    );
    assert_eq!(
        sim.live_disk_manifest(session),
        Some(ManifestRef {
            manifest_id: m1,
            version: 1
        }),
        "the retry did not roll back or corrupt the pointer"
    );

    // A genuine forward publish (v2) advances the pointer.
    assert_eq!(
        sim.publish_manifest(session, sandbox, m2, 2).await,
        LiveManifestPublishOutcome::Applied,
        "a fresh manifest version advances"
    );
    assert_eq!(
        sim.live_disk_manifest(session),
        Some(ManifestRef {
            manifest_id: m2,
            version: 2
        }),
        "the pointer advanced to v2"
    );

    // Now the binding departs — ADR 0101 C: the evict op leaves it bound;
    // the finalize's recoverable row + settle is what detaches.
    sim.advance(3600).await;
    sim.evict_to_idle(session).await;
    for _ in 0..=engram_dst_cosim::host::FINALIZE_MAX_ATTEMPTS {
        sim.finalize_pending().await;
    }
    assert_eq!(
        sim.sandbox_of(session).await,
        None,
        "the eviction settle cleared the binding"
    );

    // A late/duplicated publish for the now-departed sandbox is dropped as
    // Stale — it must NOT resurrect or clobber the session's pointer.
    assert_eq!(
        sim.publish_manifest(session, sandbox, m1, 1).await,
        LiveManifestPublishOutcome::Stale,
        "a publish from a departed binding is structurally stale, not applied"
    );
    assert_eq!(
        sim.live_disk_manifest(session),
        Some(ManifestRef {
            manifest_id: m2,
            version: 2
        }),
        "the stale publish left the last committed pointer (v2) intact"
    );
}
