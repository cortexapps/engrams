//! Non-vacuity proof for the `quiescence-recovery-wedged` oracle (an
//! oracle that can't fire is worthless — the R5-shaped canary pattern
//! from `model_oracle.rs`).
//!
//! The honest wedge construction is the exact class that motivated the
//! oracle: the #896 evac-exhaustion residue — an Idle row with its
//! source binding retained — under a teardown that can never confirm.
//! With the source host's effect queue DEFERRED, the resume gate's
//! re-issued destroy never applies and the probe keeps answering
//! alive, so every driven resume attempt fails 503-retryable until
//! `RESUME_MAX_ATTEMPTS` converts the op to a terminal `Failed`,
//! deliberately leaving the session Idle. Pre-oracle that world
//! converged green; the oracle must name it.

use engram_dst::{Profile, Sim, Step};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("current-thread runtime")
}

#[test]
fn recovery_oracle_fires_on_an_unresumable_idle_session() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(11, Profile::Calm).with_faithful_hosts();

        // Fleet up; one session booted to Active, then rested to Idle
        // the surgical way (a normal unbound Idle row — the recovery
        // target shape).
        sim.execute(Step::HostHeartbeats).await;
        sim.execute(Step::CreateSession).await;
        for h in 0..3 {
            sim.execute(Step::HostCheckpoint(h)).await;
        }
        let sid = sim
            .world
            .meta
            .with_db(|db| {
                db.sessions
                    .values()
                    .find(|r| r.session.status == engram_core::types::session::SessionState::Active)
                    .map(|r| r.session.id)
            })
            .expect("one Active session after create");
        // The #896 exhaustion residue, surgically: Idle with the source
        // binding RETAINED while the source VM stays resident.
        let source_host = sim
            .world
            .meta
            .with_db_mut(|db| {
                let row = db.sessions.get_mut(&sid).expect("session row");
                row.session.status = engram_core::types::session::SessionState::Idle;
                row.session.host_id
            })
            .expect("bound session has a source host");
        let source_idx = sim
            .world
            .host_ids
            .iter()
            .position(|h| *h == source_host)
            .expect("source host index");

        // The teardown that can never confirm: the source's effect queue
        // is deferred, so the gate's re-issued destroy never applies and
        // the probe keeps answering alive.
        sim.execute(Step::DeferHost(source_idx, true)).await;

        let err = sim
            .drive_recovery_oracle(300)
            .await
            .expect_err("an unresumable Idle session must fail the recoverability oracle");
        assert!(
            err.contains("quiescence-recovery-wedged"),
            "the wedge slug names the finding: {err}",
        );
        assert!(
            err.contains(&sid.to_string()),
            "the wedged session is named: {err}",
        );
        assert!(
            err.contains("[Failed]"),
            "the detail names the terminally-failed resume op: {err}",
        );
    });
}

#[test]
fn quiescence_rejects_open_teleports_and_retired_host_work() {
    use engram_core::traits::{Clock, Entropy, MetadataStore};
    use engram_core::types::teleport::*;
    use engram_dst::invariants;
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
        let now = sim.world.clock.now_utc();
        sim.world.meta.with_db_mut(|db| {
            db.teleports.insert(
                id,
                TeleportRow {
                    id,
                    session_id: session.id,
                    kind: TeleportKind::Snapshot,
                    reason: TeleportReason::RetireHost,
                    phase: TeleportPhase::Admitted,
                    source_host_id: source,
                    source_sandbox_id: session.sandbox_id.unwrap(),
                    dest_host_id: dest,
                    dest_sandbox_id: None,
                    pinned_dest: false,
                    mem_budget_mib: 1,
                    cpu_budget_vcpus: 1,
                    snapshot_id: None,
                    export_id: None,
                    live_payload: None,
                    attempts: 0,
                    error: None,
                    created_at: now,
                    updated_at: now,
                    finished_at: None,
                },
            );
        });
        assert_eq!(
            invariants::check_quiescence(&sim.world)
                .unwrap_err()
                .invariant,
            "quiescence-open-teleport"
        );
        sim.world.meta.with_db_mut(|db| {
            db.teleports.remove(&id);
            db.hosts.get_mut(&source).unwrap().status = engram_core::types::HostStatus::Retired;
        });
        assert_eq!(
            invariants::check_quiescence(&sim.world)
                .unwrap_err()
                .invariant,
            "quiescence-retired-host-owned"
        );
        sim.world.meta.with_db_mut(|db| {
            db.sessions.get_mut(&session.id).unwrap().session.host_id = None;
        });
        sim.world
            .meta
            .record_sandbox_tombstone(source, session.sandbox_id.unwrap(), Some(session.id))
            .await
            .unwrap();
        assert_eq!(
            invariants::check_quiescence(&sim.world)
                .unwrap_err()
                .invariant,
            "quiescence-retired-host-owned"
        );
    });
}
