//! ADR 0098 R2 — the expected-state model oracle, and its NON-VACUITY
//! proof (the R5-shaped canary: an oracle that can't fire is worthless).
//!
//! The auditor is fed only by acked outcomes: a create that fully
//! established (reached Active with a durable row). We drive one, confirm
//! the auditor is clean, then DROP its row directly (the corruption a
//! coordinator-unbind-vs-teardown race — #570 — would produce) and assert
//! the auditor FIRES; restoring the row clears it.

use engram_dst::{Profile, Sim, Step};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("current-thread runtime")
}

#[test]
fn model_oracle_fires_on_a_dropped_live_session_row() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(7, Profile::Calm);
        // Register the fleet, then boot a session to Active — an acked-live
        // milestone the auditor records.
        sim.execute(Step::HostHeartbeats).await;
        sim.execute(Step::CreateSession).await;
        assert!(
            sim.model().tracked() >= 1,
            "a booted session must have been recorded as acked-live",
        );
        // Clean baseline.
        assert!(
            sim.model().check(&sim.world).is_ok(),
            "the auditor must pass while the live row is present",
        );

        // Pick the acked-live (Active) session and DROP its row directly.
        let sid = sim
            .world
            .meta
            .with_db(|db| {
                db.sessions
                    .values()
                    .find(|r| r.session.status == engram_core::types::session::SessionState::Active)
                    .map(|r| r.session.id)
            })
            .expect("an Active session exists");
        let saved = sim
            .world
            .meta
            .with_db_mut(|db| db.sessions.remove(&sid))
            .expect("removed the live row");

        // The auditor MUST catch the silent loss.
        let fired = sim.model().check(&sim.world);
        assert!(
            fired.is_err(),
            "the auditor must fire when an acked-live session's row vanishes",
        );
        assert_eq!(
            fired.unwrap_err().invariant,
            "model-acked-create-not-lost",
            "the right invariant fires",
        );

        // Restore → clean again (proves the fire was the corruption, not a
        // latent state bug).
        sim.world.meta.with_db_mut(|db| {
            db.sessions.insert(sid, saved);
        });
        assert!(
            sim.model().check(&sim.world).is_ok(),
            "restoring the row clears the auditor",
        );
    });
}

#[test]
fn capacity_oracle_counts_an_open_teleport_destination() {
    use engram_core::traits::{Clock, Entropy, MetadataStore};
    use engram_core::types::teleport::*;
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(11, Profile::Calm).with_faithful_hosts();
        sim.execute(Step::HostHeartbeats).await;
        sim.execute(Step::CreateSession).await;
        let session = sim.world.meta.with_db(|db| {
            db.sessions
                .values()
                .find(|s| s.session.status == engram_core::types::SessionState::Active)
                .unwrap()
                .session
                .clone()
        });
        let source = session.host_id.unwrap();
        let dest = *sim.world.host_ids.iter().find(|h| **h != source).unwrap();
        let id = engram_core::TeleportId::from(sim.world.entropy.uuid());
        let result = sim
            .world
            .meta
            .teleport_admit(TeleportAdmitRequest {
                id,
                session_id: session.id,
                reason: TeleportReason::Ui,
                epoch: sim
                    .world
                    .meta
                    .with_db(|db| db.sessions[&session.id].current_epoch),
                candidates: vec![dest],
                pinned_dest: Some(dest),
                mem_budget_mib: 1,
                cpu_budget_vcpus: 1,
                max_open_per_dest: 1,
                live_capable: false,
            })
            .await
            .unwrap();
        assert!(matches!(result, TeleportAdmitOutcome::Admitted(_)));
        sim.world.meta.with_db_mut(|db| {
            db.teleports.get_mut(&id).unwrap().mem_budget_mib =
                i64::try_from(db.hosts[&dest].utilization.allocatable_mib).unwrap() + 1;
        });
        let mut oracle = engram_dst::invariants::Oracles::default();
        assert_eq!(
            oracle.check_step(&sim.world).unwrap_err().invariant,
            "placement-accounting"
        );
        sim.world.meta.with_db_mut(|db| {
            let row = db.teleports.get_mut(&id).unwrap();
            row.phase = TeleportPhase::Aborted;
            row.finished_at = Some(sim.world.clock.now_utc());
            db.sessions.get_mut(&session.id).unwrap().session.status =
                engram_core::types::SessionState::Active;
        });
        oracle
            .check_step(&sim.world)
            .expect("released teleport reservation");
    });
}
