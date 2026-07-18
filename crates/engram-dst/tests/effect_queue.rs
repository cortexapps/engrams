//! ADR 0098 R2 — the host-effect queue.
//!
//! Pre-R2 the `SimHostClient` verbs mutated world truth inline, so RPC
//! loss/reorder/duplication and a replica crash BETWEEN the store commit
//! (the ack the coordinator already holds) and the host-side world effect
//! were structurally impossible (the audit's finding #2/#4). These
//! hand-driven scenarios prove the window is now reachable and that the
//! system still converges out of it — the effect queue's regression guard.

use engram_dst::{Profile, Sim, Step};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("current-thread runtime")
}

fn world_sandbox_count(sim: &Sim) -> usize {
    sim.world
        .host_world
        .hosts
        .lock()
        .values()
        .map(|h| h.sandboxes.len())
        .sum()
}

/// A deferred host withholds its verbs' world effects from world truth
/// until an explicit delivery — the committed-but-unapplied window.
#[test]
fn deferred_create_effect_is_withheld_then_delivered() {
    let rt = rt();
    rt.block_on(async {
        tokio::time::pause();
        // Calm base: no random faults. We drive the queue by hand.
        let mut sim = Sim::new(7, Profile::Calm);
        sim.execute(Step::HostHeartbeats).await;

        // Open a deferred window on every host, then boot a session. The
        // boot's sandbox-alloc verb (restore_base_for_session) returns an
        // id — the coordinator commits its binding — but the world effect
        // is queued, not applied.
        sim.execute(Step::DeferHost(0, true)).await;
        sim.execute(Step::DeferHost(1, true)).await;
        sim.execute(Step::CreateSession).await;

        assert!(
            sim.world.host_world.pending_len() >= 1,
            "a deferred boot must have queued its create effect",
        );
        assert_eq!(
            world_sandbox_count(&sim),
            0,
            "the withheld effect must not have touched world truth yet",
        );

        // Deliver in serial order: the sandbox materializes now.
        sim.execute(Step::DeliverEffects).await;
        assert_eq!(sim.world.host_world.pending_len(), 0, "queue drained");
        assert!(
            world_sandbox_count(&sim) >= 1,
            "delivery applies the previously-withheld effect",
        );
    });
}

/// A replica crash inside the commit→effect window, followed by the acked
/// verb's world effect being LOST, still converges once the fleet heals —
/// the reachable in-flight-interruption + message-loss interleaving.
#[test]
fn crash_between_commit_and_effect_then_loss_still_converges() {
    let rt = rt();
    rt.block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(11, Profile::Calm);
        sim.execute(Step::HostHeartbeats).await;
        sim.execute(Step::DeferHost(0, true)).await;
        sim.execute(Step::DeferHost(1, true)).await;
        sim.execute(Step::CreateSession).await;
        assert!(
            sim.world.host_world.pending_len() >= 1,
            "precondition: the boot's effect is queued (window open)",
        );

        // Crash the replica while its commit is durable but the world
        // effect is still queued — the previously-impossible state.
        sim.execute(Step::CrashReplica(0)).await;
        // The acked verb's world mutation is then dropped on the floor.
        sim.execute(Step::DropEffect).await;

        // Heal + quiesce: no_op_dropped + no-stragglers + no-orphans must
        // all still hold (the surviving replica reconciles the session out).
        if let Err(msg) = sim.run(0).await {
            panic!("effect-queue interleaving failed to converge: {msg}");
        }
    });
}
