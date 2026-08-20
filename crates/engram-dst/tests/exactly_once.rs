//! ADR 0108 A6 — the exactly-once landing condition, pinned BEFORE the
//! behavior change ships. `run_started{prompt_id}` must fire exactly
//! once per prompt across the spawn-carried rail, the outbox rail,
//! redelivery, and their interleavings. The sim guest models the
//! harness's `seen_prompt_ids` dedup; the spawn-carried prompt rides
//! the `ENGRAM_INITIAL_PROMPT*` env keys on `start_agent`'s AgentSpec
//! and is delivered when the dial completes (a harness cannot run a
//! prompt before it exists). The coordinator does not stamp those keys
//! yet — these scenarios drive `start_agent` directly, so the oracle
//! and the world model land green on the current outbox-only path.
//!
//! Scenarios hand-drive `Sim::execute` (the pinned-regression pattern).
//! Everything runs on the paused tokio clock; no wall-clock sleeps.

use std::time::Duration;

use engram_core::traits::metadata::{CreateDisposition, SessionCreateWriteSet};
use engram_core::traits::Entropy as _;
use engram_core::traits::HostClient as _;
use engram_core::traits::SessionFence;
use engram_core::types::sandbox::AgentSpec;
use engram_core::types::session::{SessionMode, SessionSpec, SessionState};
use engram_core::types::session_op::{EnqueueOutcome, OpKind};
use engram_core::{HostId, SandboxId, SessionId};
use engram_dst::scheduler::SIM_IMAGE;
use engram_dst::{invariants, workload, DriverKind, Profile, Sim, Step};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("current-thread runtime")
}

/// Reserve + boot one session to Active with the real CreateBoot op
/// (the ttft_attach boot shape). `create_prompt`: `Some` enqueues the
/// prompt while the session is still Pending — outbox row + Deliver op
/// exist before the boot completes.
async fn boot_session(
    sim: &mut Sim,
    create_prompt: Option<&str>,
) -> (SessionId, SandboxId, HostId) {
    sim.execute(Step::HostHeartbeats).await;
    let state = sim.world.replicas[0]
        .state
        .clone()
        .expect("replica 0 is up");
    let session_id = SessionId::from(sim.world.entropy.uuid());
    let ws = SessionCreateWriteSet {
        session_id,
        spec: SessionSpec {
            image: SIM_IMAGE.into(),
            mode: SessionMode::DevVm,
        },
        mem_budget_mib: 2048,
        cpu_budget_vcpus: 2,
        sealed_secrets: None,
        capabilities: Vec::new(),
        integration_policy_json: None,
        runtime_spec: engram_core::types::runtime_spec::RuntimeSpec::new(
            Vec::new(),
            None,
            None,
            Vec::new(),
        ),
        oauth_binding: None,
    };
    let disp = state
        .services
        .meta
        .reserve_and_persist_create(ws, &sim.world.host_ids, 0)
        .await
        .expect("reserve_and_persist_create");
    assert!(
        matches!(disp, CreateDisposition::Placed(_)),
        "the fresh sim fleet must fit the create"
    );
    if let Some(prompt_id) = create_prompt {
        assert!(
            workload::api_prompt(&state, session_id, prompt_id).await,
            "the create-time prompt must ack"
        );
        workload::drain_detached().await;
    }
    match engram_coordinator::session_ops::enqueue_claim(
        &state,
        session_id,
        OpKind::CreateBoot,
        serde_json::json!({}),
        Some(&format!("create:{session_id}")),
    )
    .await
    {
        Ok(EnqueueOutcome::Claimed(op)) => {
            engram_coordinator::session_ops::drive_claimed(&state, op).await;
        }
        Ok(EnqueueOutcome::Queued(_)) => {
            engram_coordinator::session_ops::drive_session(&state, session_id).await;
        }
        Ok(EnqueueOutcome::Duplicate) => panic!("fresh create key cannot be a duplicate"),
        Err(e) => panic!("create_boot enqueue failed: {e}"),
    }
    workload::drain_detached().await;
    let (status, sandbox, host) = sim.world.meta.with_db(|db| {
        let row = db.sessions.get(&session_id).expect("session row");
        (
            row.session.status,
            row.session.sandbox_id,
            row.session.host_id,
        )
    });
    assert_eq!(status, SessionState::Active, "boot must land Active");
    (
        session_id,
        sandbox.expect("bound sandbox"),
        host.expect("bound host"),
    )
}

fn host_client(sim: &Sim, host: HostId) -> engram_dst::world::SimHostClient {
    engram_dst::world::SimHostClient {
        host_id: host,
        world: sim.world.host_world.clone(),
        entropy: sim.world.entropy.clone(),
    }
}

/// An AgentSpec carrying the ADR 0108 A6 spawn-prompt env keys — the
/// shape the coordinator's boot pipeline stamps in the follow-up PR.
fn agent_spec_with_prompt(prompt_id: &str, text: &str) -> AgentSpec {
    let mut env = std::collections::HashMap::new();
    env.insert(
        engram_harness_proto::INITIAL_PROMPT_ENV.to_string(),
        text.to_string(),
    );
    env.insert(
        engram_harness_proto::INITIAL_PROMPT_ID_ENV.to_string(),
        prompt_id.to_string(),
    );
    AgentSpec {
        argv: Vec::new(),
        env,
        session_env: std::collections::HashMap::new(),
        binding_epoch: 1,
        host_ca_pem: None,
    }
}

fn egress_policy(session_id: SessionId, sandbox_id: SandboxId) -> SessionEgressPolicy {
    SessionEgressPolicy {
        session_id,
        sandbox_id,
        guest_ip: std::net::Ipv4Addr::UNSPECIFIED,
        network_allow_hosts: Vec::new(),
        network_allow_host_patterns: Vec::new(),
        allow_all: false,
        secrets: Vec::new(),
        injects: Vec::new(),
        observes: Vec::new(),
        guest_services: Vec::new(),
        tunnels: Vec::new(),
        apps: Vec::new(),
        secret_mode: engram_core::types::image::SecretMode::Broker,
    }
}
use engram_core::types::egress::SessionEgressPolicy;

fn prompt_acked(sim: &Sim, prompt_id: &str) -> bool {
    sim.world.meta.with_db(|db| {
        db.outbox
            .get(prompt_id)
            .is_some_and(|r| r.acked_at.is_some())
    })
}

fn run_started_count(sim: &Sim, sid: SessionId, prompt_id: &str) -> usize {
    sim.world.meta.with_db(|db| {
        db.session_events
            .get(&sid)
            .map(|events| {
                events
                    .iter()
                    .filter(|ev| {
                        ev.kind == "run_started"
                            && ev.payload.get("prompt_id").and_then(|v| v.as_str())
                                == Some(prompt_id)
                    })
                    .count()
            })
            .unwrap_or(0)
    })
}

/// The A6 happy path: the spawn-carried prompt runs the moment the
/// harness attaches, its `run_started` acks the outbox row, and the
/// backstop Deliver op finds nothing left to do — exactly one run.
#[test]
fn spawn_carried_prompt_runs_exactly_once_with_the_outbox_backstop() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(0x0A61, Profile::Calm);
        sim.world.host_world.set_attach_announce(true);
        let prompt_id = "a6-spawn-prompt";
        let (sid, sandbox, host) = boot_session(&mut sim, Some(prompt_id)).await;
        assert!(!prompt_acked(&sim, prompt_id), "nothing has run yet");

        // The follow-up PR's coordinator stamp, hand-driven: start_agent
        // carries the prompt env. (Also a SIGUSR1-shaped nudge — the
        // in-flight boot dial is replaced, as a real reattach would.)
        let client = host_client(&sim, host);
        client
            .start_agent(
                sandbox,
                agent_spec_with_prompt(prompt_id, "hello from the spawn rail"),
                egress_policy(sid, sandbox),
                SessionFence::unfenced(),
            )
            .await
            .expect("start_agent");

        // The dial completes; the pump records the spawn prompt and
        // echoes run_started through the real ingestion route.
        sim.execute(Step::AdvanceTime(Duration::from_millis(300)))
            .await;
        assert!(sim.world.host_world.harness_attached(sandbox));
        assert!(prompt_acked(&sim, prompt_id), "the spawn echo acks the row");
        assert_eq!(run_started_count(&sim, sid, prompt_id), 1);

        // The backstop: the create-time Deliver op (deferred while the
        // session booted) retries, finds the row acked, and completes.
        // A redelivery would be deduped by the guest's seen-set anyway.
        sim.execute(Step::AdvanceTime(Duration::from_secs(2))).await;
        sim.execute(Step::Driver(0, DriverKind::SessionOps)).await;
        sim.execute(Step::Driver(0, DriverKind::OutboxDelivery))
            .await;
        assert_eq!(
            run_started_count(&sim, sid, prompt_id),
            1,
            "the outbox rail must not produce a second run"
        );
        invariants::run_started_exactly_once(&sim.world).expect("oracle holds");
        invariants::check_quiescence(&sim.world).expect("quiescent");
    });
}

/// The stale-bundle skew shape: the placed host's harness predates the
/// A6 consumer and silently ignores the spawn env. Delivery falls back
/// to the outbox rail — still exactly once.
#[test]
fn stale_bundle_host_falls_back_to_the_outbox_rail() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(0x0A62, Profile::Calm);
        sim.world.host_world.set_attach_announce(true);
        let prompt_id = "a6-stale-bundle-prompt";
        // Arm the fault fleet-wide before any spawn.
        for host in sim.world.host_ids.clone() {
            sim.world.host_world.set_drop_spawn_prompt(host, true);
        }
        let (sid, sandbox, host) = boot_session(&mut sim, Some(prompt_id)).await;
        let client = host_client(&sim, host);
        client
            .start_agent(
                sandbox,
                agent_spec_with_prompt(prompt_id, "dropped by the stale bundle"),
                egress_policy(sid, sandbox),
                SessionFence::unfenced(),
            )
            .await
            .expect("start_agent");

        // Attach completes with NO spawn echo; the A3 Idle announcement
        // wakes the deferred Deliver op, which forwards over the wire.
        sim.execute(Step::AdvanceTime(Duration::from_millis(300)))
            .await;
        assert!(sim.world.host_world.harness_attached(sandbox));
        assert!(
            !prompt_acked(&sim, prompt_id),
            "the dropped spawn prompt must not have run"
        );
        sim.execute(Step::Driver(0, DriverKind::SessionOps)).await;
        assert!(
            prompt_acked(&sim, prompt_id),
            "the outbox rail delivered after the attach"
        );
        assert_eq!(run_started_count(&sim, sid, prompt_id), 1);
        invariants::run_started_exactly_once(&sim.world).expect("oracle holds");
    });
}

/// A duplicate forward (the at-least-once outbox redelivery shape) is
/// accepted on the wire and deduped by the guest's seen-set: one echo.
#[test]
fn duplicate_forward_echoes_exactly_once() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(0x0A63, Profile::Calm);
        sim.world.host_world.set_attach_announce(true);
        let (sid, sandbox, host) = boot_session(&mut sim, None).await;
        sim.execute(Step::AdvanceTime(Duration::from_millis(300)))
            .await;
        assert!(sim.world.host_world.harness_attached(sandbox));

        let state = sim.world.replicas[0]
            .state
            .clone()
            .expect("replica 0 is up");
        let prompt_id = "a6-dup-forward-prompt";
        assert!(workload::api_prompt(&state, sid, prompt_id).await);
        workload::drain_detached().await;

        // A second forward of the same id — the redelivery shape.
        let client = host_client(&sim, host);
        client
            .send_prompt(sandbox, prompt_id.into(), "dup".into(), None)
            .await
            .expect("attached harness accepts the duplicate");
        sim.execute(Step::AdvanceTime(Duration::from_millis(10)))
            .await;

        assert!(prompt_acked(&sim, prompt_id));
        assert_eq!(
            run_started_count(&sim, sid, prompt_id),
            1,
            "the guest dedup collapses the duplicate to one run"
        );
        invariants::run_started_exactly_once(&sim.world).expect("oracle holds");
    });
}

/// NON-VACUITY: with the guest dedup disabled, the same duplicate
/// forward double-echoes and the oracle FIRES. An oracle that cannot
/// fire is worthless (ADR 0099 discipline).
#[test]
fn double_delivery_without_guest_dedup_trips_the_oracle() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(0x0A64, Profile::Calm);
        sim.world.host_world.set_attach_announce(true);
        sim.world.host_world.set_guest_dedup(false);
        let (sid, sandbox, host) = boot_session(&mut sim, None).await;
        sim.execute(Step::AdvanceTime(Duration::from_millis(300)))
            .await;

        let state = sim.world.replicas[0]
            .state
            .clone()
            .expect("replica 0 is up");
        let prompt_id = "a6-vacuity-prompt";
        assert!(workload::api_prompt(&state, sid, prompt_id).await);
        workload::drain_detached().await;
        let client = host_client(&sim, host);
        client
            .send_prompt(sandbox, prompt_id.into(), "dup".into(), None)
            .await
            .expect("accepted");
        sim.execute(Step::AdvanceTime(Duration::from_millis(10)))
            .await;

        assert_eq!(
            run_started_count(&sim, sid, prompt_id),
            2,
            "dedup off: the duplicate double-echoes"
        );
        let violation = invariants::run_started_exactly_once(&sim.world)
            .expect_err("a double run_started must fire the oracle");
        assert_eq!(violation.invariant, "run-started-exactly-once");
    });
}
