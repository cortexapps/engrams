//! ADR 0110 inverts the old roll quarantine tests.
//! Those tests proved that the quarantine path did not destroy a survivor.
//! These tests prove that a clean roll emits no rollback and rewinds no event index.

use engram_coordinator::state::SessionEvent;
use engram_core::types::event::PersistedEvent;
use engram_core::SessionId;
use engram_dst_cosim::Cosim;

async fn add_guest_event(sim: &Cosim, session: SessionId, exec_id: &str) -> i64 {
    sim.world
        .state
        .emit(
            session,
            SessionEvent::Stdout {
                exec_id: exec_id.to_string(),
                chunk: "guest work before roll\n".to_string(),
                bytes_start: 0,
                bytes_end: 23,
            },
        )
        .await
        .expect("persist the guest event")
}

fn session_events(sim: &Cosim, session: SessionId) -> Vec<PersistedEvent> {
    sim.world
        .meta
        .with_db(|db| db.session_events.get(&session).cloned().unwrap_or_default())
}

fn assert_no_rollbacks_or_rewinds(
    sim: &Cosim,
    session: SessionId,
    before_roll: &[PersistedEvent],
    before_roll_max_idx: i64,
) {
    let after_roll = session_events(sim, session);

    let rollback_rows: Vec<_> = after_roll
        .iter()
        .filter(|event| event.kind == "durability_rollback")
        .collect();
    assert!(
        rollback_rows.is_empty(),
        "a roll must not create durability_rollback rows: {rollback_rows:?}"
    );

    let rewound_indexes: Vec<_> = after_roll
        .iter()
        .filter(|event| event.rewound_at.is_some())
        .map(|event| event.idx)
        .collect();
    assert!(
        rewound_indexes.is_empty(),
        "a roll must not rewind event indexes: {rewound_indexes:?}"
    );

    let after_pre_roll: Vec<_> = after_roll
        .iter()
        .filter(|event| event.idx <= before_roll_max_idx)
        .collect();
    assert_eq!(
        after_pre_roll.len(),
        before_roll.len(),
        "all pre-roll event indexes must remain present"
    );
    for before_event in before_roll {
        let after_event = after_pre_roll
            .iter()
            .find(|event| event.idx == before_event.idx)
            .unwrap_or_else(|| panic!("pre-roll event index {} is missing", before_event.idx));
        assert_eq!(
            serde_json::to_value(*after_event).expect("serialize the current event"),
            serde_json::to_value(before_event).expect("serialize the pre-roll event"),
            "pre-roll event index {} changed during the roll",
            before_event.idx
        );
    }
}

#[tokio::test(start_paused = true)]
async fn listed_survivor_roll_rehydrates_without_rollback_or_rewind() {
    let mut sim = Cosim::new(0x0110_0001).await;
    let session = sim.boot_session().await;
    sim.guest_work(session, 2).await;
    let guest_event_idx = add_guest_event(&sim, session, "exec:adr-0110:one-roll").await;

    sim.park(session).await;
    assert!(sim.is_parked(session).await, "the survivor is parked");
    let before_roll = session_events(&sim, session);
    let before_roll_max_idx = before_roll
        .iter()
        .map(|event| event.idx)
        .max()
        .expect("the session has pre-roll events");
    assert_eq!(
        before_roll_max_idx, guest_event_idx,
        "the guest event is the pre-roll log head"
    );

    sim.roll_host().await;
    assert!(
        !sim.served_by_current(session).await,
        "the new generation has not served the survivor yet"
    );
    sim.register_rehydrate(true).await;
    assert!(
        sim.served_by_current(session).await,
        "the coordinator-listed survivor rehydrates"
    );
    assert!(sim.unpause(session).await, "the survivor resumes");

    assert_no_rollbacks_or_rewinds(&sim, session, &before_roll, before_roll_max_idx);
    sim.assert_no_severed_live_holder().await.unwrap();
    sim.assert_slot_accounting().await.unwrap();
    sim.assert_ownership_agreement().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn repeated_rehydrate_passes_do_not_create_rollback_or_rewind() {
    let mut sim = Cosim::new(0x0110_0002).await;
    let session = sim.boot_session().await;
    sim.guest_work(session, 3).await;
    let guest_event_idx = add_guest_event(&sim, session, "exec:adr-0110:repeated-rehydrate").await;

    sim.park(session).await;
    assert!(sim.is_parked(session).await, "the survivor is parked");
    let before_roll = session_events(&sim, session);
    let before_roll_max_idx = before_roll
        .iter()
        .map(|event| event.idx)
        .max()
        .expect("the session has pre-roll events");
    assert_eq!(
        before_roll_max_idx, guest_event_idx,
        "the guest event is the pre-roll log head"
    );

    sim.roll_host().await;
    for _ in 0..5 {
        sim.register_rehydrate(true).await;
        sim.advance(5).await;
    }
    assert!(
        sim.served_by_current(session).await,
        "repeated passes keep the listed survivor served"
    );
    assert!(sim.unpause(session).await, "the survivor resumes");

    assert_no_rollbacks_or_rewinds(&sim, session, &before_roll, before_roll_max_idx);
    sim.assert_no_severed_live_holder().await.unwrap();
    sim.assert_slot_accounting().await.unwrap();
    sim.assert_ownership_agreement().await.unwrap();
}
