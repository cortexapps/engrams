//! ADR 0098 R2 — the API-driven workload over the REAL surface.
//!
//! Pre-R2 the workload called store/core fns directly (0/17 HTTP, 0/~69
//! gRPC driven — the audit's finding). These tests drive a replica's
//! ACTUAL surface: the tonic `AppSessionService`/`AppFleetService` impls
//! (auth + convert.rs + the same `*_core` the axum handler calls) and the
//! actual axum `api::router` via `tower::ServiceExt::oneshot`. See
//! `workload.rs` for the real-wire vs handler-direct honesty table.
//!
//! (Folding this workload into the chaos/calm SWARM additionally requires
//! host-fidelity fixes — schedulable `wire_version`, staged `ready_images`
//! — that unmask a genuine, separate driver-double-boot / eviction-
//! durability class the pre-R2 sim silently suppressed; those are captured
//! as R2 findings for a follow-up, so here the surface is exercised
//! directly and deterministically instead.)

use engram_coordinator::state::SharedState;
use engram_dst::scheduler::SIM_IMAGE;
use engram_dst::workload;
use engram_dst::{Profile, Sim};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("current-thread runtime")
}

fn a_replica(sim: &Sim) -> SharedState {
    sim.world
        .replicas
        .iter()
        .find_map(|r| r.state.clone())
        .expect("a live replica")
}

/// The real gRPC `create_session` persists the acked session, and the
/// session id is DETERMINISTIC across replays — the ADR 0098 D1 fix (the
/// handler used to mint the id via a raw `SessionId::new()`, so it
/// diverged every run; it now reads the injected entropy).
#[test]
fn api_create_is_acked_persisted_and_deterministic() {
    let create = |seed: u64| {
        rt().block_on(async move {
            tokio::time::pause();
            let sim = Sim::new(seed, Profile::Calm);
            let st = a_replica(&sim);
            let ack = workload::api_create(&st, SIM_IMAGE)
                .await
                .expect("create acked");
            // created-never-lost: the acked session exists in the store.
            let present = sim
                .world
                .meta
                .with_db(|db| db.sessions.contains_key(&ack.session_id));
            assert!(present, "an acked-created session must exist in the store");
            ack.session_id
        })
    };
    // Same seed → byte-identical acked id (the determinism fix); distinct
    // seeds → distinct ids (the injected stream is actually seeded).
    assert_eq!(create(7), create(7), "acked session id must replay");
    assert_ne!(
        create(7),
        create(8),
        "distinct seeds must mint distinct ids"
    );
}

/// Prompt + delete over the real gRPC surface: each acks, and delete's
/// Destroy op / teardown is observable in the store (exercising the
/// broker-token + teleport-target SimMeta methods the delete path hits).
#[test]
fn api_prompt_then_delete_round_trip() {
    rt().block_on(async {
        tokio::time::pause();
        let sim = Sim::new(3, Profile::Calm);
        let st = a_replica(&sim);
        let ack = workload::api_create(&st, SIM_IMAGE)
            .await
            .expect("create acked");
        let sid = ack.session_id;

        // A prompt on the (queued) session is durably accepted (ADR 0073
        // outbox) — the coordinator acks and an outbox row appears.
        assert!(
            workload::api_prompt(&st, sid, "sim-prompt-1").await,
            "send_prompt must ack",
        );
        let has_outbox = sim.world.meta.with_db(|db| !db.outbox.is_empty());
        assert!(has_outbox, "the acked prompt must persist an outbox row");

        // Delete acks and drives the session toward teardown (a Destroy op
        // is enqueued or the row is already gone).
        assert!(workload::api_delete(&st, sid).await, "delete must ack");
        let (destroy_op, row_gone) = sim.world.meta.with_db(|db| {
            let destroy = db.session_ops.values().any(|o| {
                o.session_id == sid && o.kind == engram_core::types::session_op::OpKind::Destroy
            });
            (destroy, !db.sessions.contains_key(&sid))
        });
        assert!(
            destroy_op || row_gone,
            "delete must enqueue a Destroy op or remove the row",
        );
    });
}

/// The real axum Router over `tower::oneshot`: host-facing harness-event
/// ingestion returns 204, and admin pause of a session with no live
/// sandbox is HANDLED (a 409 from the real handler, never a panic) — the
/// real-wire HTTP path, middleware and extractors included.
#[test]
fn api_http_router_real_wire() {
    rt().block_on(async {
        tokio::time::pause();
        let sim = Sim::new(5, Profile::Calm);
        let st = a_replica(&sim);
        let ack = workload::api_create(&st, SIM_IMAGE)
            .await
            .expect("create acked");
        // These drive the actual `api::router` via oneshot; the assertion
        // is that the real handlers run to a normal response (no panic, no
        // hang) — harness-idle ingest is the #775 eviction trigger's wire.
        workload::api_emit_harness_idle(
            &st,
            ack.session_id,
            engram_core::SandboxId::from(uuid::Uuid::from_u128(0xB0B0)),
            chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        )
        .await;
        workload::api_pause_then_resume(&st, ack.session_id).await;
    });
}
