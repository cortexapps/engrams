//! Non-vacuity proof for the `user-input-never-rewound` oracle, and
//! the directed pin for the now-live rewind path (an oracle that can't
//! fire is worthless — the R5 canary pattern from `model_oracle.rs`).
//!
//! `Step::HostCheckpoint` stamps `events_cursor` (as the production
//! checkpoint writers do), so a resume with guest history past the
//! cursor runs a REAL rung-1 rewind in the swarm. This test drives the
//! exact incident interleaving (prod 2026-08-03, session aa0829b0):
//! user input AND guest history land after the checkpoint cursor; the
//! resume must tombstone exactly the guest rows, keep every
//! user-authored row, and emit exactly one
//! `recovered_from_checkpoint{cause: checkpoint_lag}` with an honest
//! `rolled_back` count.

use engram_dst::{Profile, Sim, Step};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("current-thread runtime")
}

#[test]
fn resume_rewinds_guest_history_but_never_user_input() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(17, Profile::Calm).with_faithful_hosts();

        sim.execute(Step::HostHeartbeats).await;
        sim.execute(Step::CreateSession).await;
        let state = sim.world.replicas[0]
            .state
            .clone()
            .expect("replica 0 is up");
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
        let source_host = sim
            .world
            .meta
            .with_db(|db| db.sessions.get(&sid).and_then(|r| r.session.host_id))
            .expect("bound session has a host");
        let host_idx = sim
            .world
            .host_ids
            .iter()
            .position(|h| *h == source_host)
            .expect("host index");

        // Pre-checkpoint history (covered by the cursor).
        let meta = &state.services.meta;
        meta.append_session_event(
            sid,
            "agent_message",
            serde_json::json!({"role": "assistant", "text": "covered work"}),
        )
        .await
        .expect("append covered row");

        // The checkpoint stamps `events_cursor` at this point.
        sim.execute(Step::HostCheckpoint(host_idx)).await;

        // Post-cursor: GUEST history (must roll back) interleaved with
        // USER input (must survive) — the incident shape.
        meta.append_session_event(sid, "run_started", serde_json::json!({"run_id": "r-post"}))
            .await
            .expect("append post-cursor run");
        meta.append_session_event(
            sid,
            "agent_message",
            serde_json::json!({"role": "user", "text": "ok resume", "prompt_id": "p-race"}),
        )
        .await
        .expect("append user echo");
        meta.append_session_event(
            sid,
            "agent_message",
            serde_json::json!({"role": "assistant", "text": "uncovered work"}),
        )
        .await
        .expect("append post-cursor assistant row");
        meta.append_session_event(
            sid,
            "file_shared",
            serde_json::json!({"caption": "notes.md", "artifact_id": "art-1"}),
        )
        .await
        .expect("append file_shared");

        // Rest to Idle the surgical way: a normal unbound Idle row (the
        // eviction detaches the binding; the snapshot row above is the
        // durable copy the resume selects).
        sim.world.meta.with_db_mut(|db| {
            let row = db.sessions.get_mut(&sid).expect("session row");
            row.session.status = engram_core::types::session::SessionState::Idle;
            row.session.sandbox_id = None;
            row.session.host_id = None;
        });

        // The real Resume op, driven inline to completion.
        sim.execute(Step::ResumeSession).await;
        let status = sim
            .world
            .meta
            .with_db(|db| db.sessions.get(&sid).map(|r| r.session.status))
            .expect("session row");
        assert_eq!(
            status,
            engram_core::types::session::SessionState::Active,
            "the directed resume must converge",
        );

        // The rewind really ran, and it was provenance-exact.
        sim.world.meta.with_db(|db| {
            let events = db.session_events.get(&sid).expect("event log");
            let tombstoned: Vec<&str> = events
                .iter()
                .filter(|e| e.rewound_at.is_some())
                .map(|e| e.kind.as_str())
                .collect();
            assert_eq!(
                tombstoned.len(),
                2,
                "exactly the post-cursor guest rows roll back: {tombstoned:?}",
            );
            assert!(tombstoned.contains(&"run_started"));
            assert!(tombstoned.contains(&"agent_message"));
            for e in events {
                let user_input = matches!(e.kind.as_str(), "file_shared")
                    || (e.kind == "agent_message"
                        && e.payload.get("role").and_then(|v| v.as_str()) == Some("user"));
                if user_input {
                    assert!(
                        e.rewound_at.is_none(),
                        "user input survived the rewind: idx {} kind {}",
                        e.idx,
                        e.kind,
                    );
                }
            }
            let recoveries: Vec<_> = events
                .iter()
                .filter(|e| e.kind == "recovered_from_checkpoint")
                .collect();
            assert_eq!(
                recoveries.len(),
                1,
                "genuine lag fires exactly one recovery event",
            );
            assert_eq!(
                recoveries[0].payload.get("cause").and_then(|v| v.as_str()),
                Some("checkpoint_lag"),
            );
            assert_eq!(
                recoveries[0]
                    .payload
                    .get("rolled_back")
                    .and_then(|v| v.as_u64()),
                Some(2),
                "rolled_back counts only guest history",
            );
        });
    });
}
