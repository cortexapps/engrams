//! ADR 0098 wave 4 — the API workload verbs folded into the swarm, and
//! the NON-VACUITY proofs for the two new model-oracle assertions.
//!
//! `#786` wired the real gRPC/HTTP surface but drove it only in dedicated
//! tests; wave 4 folds `Prompt` / `Rename` / `Destroy` / `DrainHost` into
//! both swarm profiles. These tests prove (a) each new `Step` drives its
//! REAL handler to an observable world mutation, and (b) the two new
//! auditor assertions actually FIRE on the corruption they name (an oracle
//! that can't fire is worthless — the R5 canary shape).

use engram_core::types::session::SessionState;
use engram_dst::model::ModelState;
use engram_dst::{Profile, Sim, Step};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("current-thread runtime")
}

/// Boot exactly one session to Active and return its id (the shared
/// fixture — mirrors `model_oracle.rs`). The one-second advance lets the
/// harness dial complete (ADR 0108 E: attach lands ~200 ms after
/// `start_agent`) so `Step::Prompt`, which targets attached sessions,
/// finds it.
async fn boot_one_active(sim: &mut Sim) -> engram_core::SessionId {
    sim.execute(Step::HostHeartbeats).await;
    sim.execute(Step::CreateSession).await;
    sim.execute(Step::AdvanceTime(std::time::Duration::from_secs(1)))
        .await;
    sim.world
        .meta
        .with_db(|db| {
            db.sessions
                .values()
                .find(|r| r.session.status == SessionState::Active)
                .map(|r| r.session.id)
        })
        .expect("one Active session after create")
}

/// Each new Step drives its REAL service handler to an observable world
/// mutation: a prompt persists an outbox row, a rename materializes the
/// title, a drain cordons the host, a destroy tears the session down.
#[test]
fn workload_verbs_drive_real_handlers() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(11, Profile::Calm).with_faithful_hosts();
        let sid = boot_one_active(&mut sim).await;

        // Prompt → a durable outbox row (the real send_prompt handler).
        sim.execute(Step::Prompt).await;
        let has_outbox = sim.world.meta.with_db(|db| !db.outbox.is_empty());
        assert!(has_outbox, "Step::Prompt must persist an outbox row");

        // Rename → the coordinator materializes suggested_title (real-wire
        // harness-title ingestion). The model recorded it confirmed-at-write.
        sim.execute(Step::Rename).await;
        let title = sim.world.meta.with_db(|db| {
            db.sessions
                .get(&sid)
                .and_then(|r| r.session.suggested_title.clone())
        });
        assert!(
            title.is_some(),
            "Step::Rename must materialize a suggested_title",
        );

        // Destroy → the Active session reaches a terminal state or its row is
        // gone (the real Destroy op + teardown). Done BEFORE the drain, since
        // Destroy targets Active-only and the drain moves the session off it.
        sim.execute(Step::Destroy).await;
        let torn_down = sim.world.meta.with_db(|db| {
            db.sessions
                .get(&sid)
                .map(|r| r.session.status.is_terminal())
                .unwrap_or(true)
        });
        assert!(
            torn_down,
            "Step::Destroy must drive the session terminal or remove its row",
        );

        // DrainHost (a FRESH bound session) → the host is cordoned (durable,
        // ADR 0047) and its bound session is evacuated off Active.
        let sid2 = boot_one_active(&mut sim).await;
        let host2 = sim
            .world
            .meta
            .with_db(|db| db.sessions.get(&sid2).and_then(|r| r.session.host_id))
            .expect("the second Active session is bound to a host");
        let host2_idx = sim
            .world
            .host_ids
            .iter()
            .position(|h| *h == host2)
            .expect("bound host is a known host");
        sim.execute(Step::DrainHost(host2_idx)).await;
        let (cordoned, off_active) = sim.world.meta.with_db(|db| {
            let cordoned = db.hosts.get(&host2).map(|h| h.cordoned).unwrap_or(false);
            let off_active = db
                .sessions
                .get(&sid2)
                .map(|r| r.session.status != SessionState::Active)
                .unwrap_or(true);
            (cordoned, off_active)
        });
        assert!(cordoned, "Step::DrainHost must cordon the drained host");
        assert!(
            off_active,
            "Step::DrainHost must move its bound session off Active (evacuation)",
        );

        // And the whole thing still converges once the fleet heals (the
        // heal uncordons the drained host so the evacuated leg re-homes).
        if let Err(msg) = sim.run(0).await {
            panic!("post-verb convergence failed: {msg}");
        }
    });
}

/// NON-VACUITY: the acked-destroy-never-resurrects oracle FIRES when a
/// torn-down session comes back to life, and CLEARS when it is terminal.
#[test]
fn model_oracle_fires_when_a_destroyed_session_resurrects() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(13, Profile::Calm).with_faithful_hosts();
        let sid = boot_one_active(&mut sim).await;
        let image = sim
            .world
            .meta
            .with_db(|db| db.sessions.get(&sid).map(|r| r.session.image.clone()))
            .expect("the Active session has an image");

        // A model we fully control: the session was acked-live, then an
        // acked destroy retired it.
        let mut model = ModelState::default();
        model.record_live(sid, image);
        model.record_destroy_acked(sid);

        // The row is still Active in the world (i.e. it resurrected /
        // was re-booted after the acked destroy) — the oracle MUST catch it.
        let fired = model.check(&sim.world);
        assert!(
            fired.is_err(),
            "the auditor must fire when a destroyed session is live again",
        );
        assert_eq!(
            fired.unwrap_err().invariant,
            "model-acked-destroy-resurrected",
            "the right invariant fires",
        );

        // Flip the row terminal (the destroy's real end state) → clean.
        sim.world.meta.with_db_mut(|db| {
            db.sessions.get_mut(&sid).unwrap().session.status = SessionState::Dead;
        });
        assert!(
            model.check(&sim.world).is_ok(),
            "a terminal destroyed session clears the auditor",
        );

        // The row vanishing entirely (hard delete) is also fine.
        sim.world.meta.with_db_mut(|db| {
            db.sessions.remove(&sid);
        });
        assert!(
            model.check(&sim.world).is_ok(),
            "a removed destroyed session clears the auditor",
        );
    });
}

/// NON-VACUITY: the acked-rename read-your-writes oracle FIRES when a
/// confirmed title is silently repainted/lost, and CLEARS on restore.
#[test]
fn model_oracle_fires_when_a_confirmed_title_is_repainted() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(17, Profile::Calm).with_faithful_hosts();
        let sid = boot_one_active(&mut sim).await;
        let image = sim
            .world
            .meta
            .with_db(|db| db.sessions.get(&sid).map(|r| r.session.image.clone()))
            .expect("the Active session has an image");

        // Materialize a title on the row, then record it as confirmed-acked.
        sim.world.meta.with_db_mut(|db| {
            db.sessions.get_mut(&sid).unwrap().session.suggested_title =
                Some("a real title".to_string());
        });
        let mut model = ModelState::default();
        model.record_live(sid, image);
        model.record_rename_acked(sid, "a real title".to_string());
        assert!(
            model.check(&sim.world).is_ok(),
            "the auditor passes while the confirmed title is present",
        );

        // Repaint the title under the live session → read-your-writes broken.
        sim.world.meta.with_db_mut(|db| {
            db.sessions.get_mut(&sid).unwrap().session.suggested_title =
                Some("a DIFFERENT title".to_string());
        });
        let fired = model.check(&sim.world);
        assert_eq!(
            fired.unwrap_err().invariant,
            "model-acked-rename-lost",
            "a repainted confirmed title fires the rename oracle",
        );

        // Losing it entirely fires too.
        sim.world.meta.with_db_mut(|db| {
            db.sessions.get_mut(&sid).unwrap().session.suggested_title = None;
        });
        assert!(
            model.check(&sim.world).is_err(),
            "a dropped confirmed title also fires",
        );

        // Restore → clean.
        sim.world.meta.with_db_mut(|db| {
            db.sessions.get_mut(&sid).unwrap().session.suggested_title =
                Some("a real title".to_string());
        });
        assert!(
            model.check(&sim.world).is_ok(),
            "restoring the confirmed title clears the auditor",
        );
    });
}
