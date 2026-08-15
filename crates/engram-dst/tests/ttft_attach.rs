//! ADR 0108 Workstream E — pinned regression scenarios for the
//! 2026-07-31 first-token-delivery incident (sessions 6f3dac3e /
//! 454c5e21), plus the non-vacuity proofs for the two new oracles
//! (`ttft-liveness`, `attach-disagreement`).
//!
//! The incident interleaving: a create carries a prompt; the Deliver op
//! wakes on the boot milestone and races the harness attach (which
//! lands 50–200 ms later), loses with `send_prompt` `NotFound`, fires
//! the destructive `start_agent` reattach into the mid-attach harness,
//! and then waits out an inflated retry backoff — 50 s to
//! `run_started`. The fixes under test (all coordinator-side, landed in
//! this tree):
//!   - A3: the first harness event (Idle) wakes the backed-off Deliver
//!     op (`harness_event_sink` → `op_wake_queued_kind`).
//!   - A4: inside the 10 s attach grace, `NotFound` defers on a fixed
//!     1 s cadence WITHOUT the destructive reattach.
//!   - A5: known waits (pre-Active, the grace) use
//!     `OpOutcome::RetryAfter` so they never inflate the backoff.
//!   - A8: fresh attach evidence recalls waiting outbox rows
//!     (`outbox_make_due` at the attach signal, before the A3 wake),
//!     and the heartbeat running-vs-attached disagreement is a REPAIR
//!     (`harness_desync::run_once` — the prod-7eddce62 stale-handle
//!     shape, pinned below).
//!
//! Scenarios hand-drive `Sim::execute` (the pinned-regression pattern —
//! an exact interleaving, not a swarm pick). Everything runs on the
//! paused tokio clock; there are no wall-clock sleeps.

use std::time::Duration;

use engram_core::traits::metadata::{CreateDisposition, SessionCreateWriteSet};
use engram_core::traits::Entropy as _;
use engram_core::types::session::{SessionMode, SessionSpec, SessionState};
use engram_core::types::session_op::{EnqueueOutcome, OpKind, OpState};
use engram_core::{HostId, SandboxId, SessionId};
use engram_dst::scheduler::SIM_IMAGE;
use engram_dst::{invariants, workload, DriverKind, Profile, Sim, Step};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("current-thread runtime")
}

/// Reserve + boot one session to Active with the real CreateBoot op.
/// `create_prompt`: `Some` enqueues the prompt while the session is
/// still Pending (the 2026-07-31 create shape — outbox row + Deliver
/// op exist BEFORE the boot completes; the deliver's first attempt
/// defers on the A5 fixed cadence); `None` boots clean (the 7eddce62
/// steady-state shape). Returns (session, sandbox, host) with the
/// session Active and the harness dial in flight.
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
        // The create-time prompt through the real gRPC handler: durable
        // outbox row + a detached Deliver op. The drain runs that op to
        // its first deferral ("session is pending" — the A5 known-wait
        // arm).
        assert!(
            workload::api_prompt(&state, session_id, prompt_id).await,
            "the create-time prompt must ack"
        );
        workload::drain_detached().await;
    }
    // The boot, inline (Step::CreateSession's machinery). Its completion
    // stamps the attach grace and wakes the sibling Deliver op (A4/ADR
    // 0094) — the exact moment the incident's race began. The prompt's
    // backed-off Deliver op sits QUEUED ahead of this enqueue, so the
    // inline claim can land `Queued`; `drive_session` then claims the
    // due head (the Deliver is gated out by its `not_before`), which is
    // the CreateBoot.
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

/// The 2026-07-31 create shape (see [`boot_session`]).
async fn create_with_prompt(sim: &mut Sim, prompt_id: &str) -> (SessionId, SandboxId, HostId) {
    boot_session(sim, Some(prompt_id)).await
}

/// The 7eddce62 steady state: session Active, harness ATTACHED, no
/// outstanding prompt. Advances past the default 200 ms dial delay so
/// the pump completes the attach (and announces Idle when the scenario
/// opted in).
async fn create_active_attached(sim: &mut Sim) -> (SessionId, SandboxId, HostId) {
    let (session_id, sandbox, host) = boot_session(sim, None).await;
    sim.execute(Step::AdvanceTime(Duration::from_millis(300)))
        .await;
    assert!(
        sim.world.host_world.harness_attached(sandbox),
        "the dial completes at the default 200 ms delay"
    );
    (session_id, sandbox, host)
}

/// The durable outbox row for a prompt (cloned world truth).
fn outbox_row(sim: &Sim, prompt_id: &str) -> engram_core::types::outbox::OutboxRow {
    sim.world
        .meta
        .with_db(|db| db.outbox.get(prompt_id).cloned())
        .expect("outbox row exists")
}

/// The most recent Deliver op row for a session:
/// (state, error, not_before).
fn deliver_op(
    sim: &Sim,
    sid: SessionId,
) -> Option<(
    OpState,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
)> {
    sim.world.meta.with_db(|db| {
        db.session_ops
            .values()
            .filter(|op| op.session_id == sid && op.kind == OpKind::Deliver)
            .max_by_key(|op| op.id)
            .map(|op| (op.state, op.error.clone(), op.not_before))
    })
}

fn prompt_acked(sim: &Sim, prompt_id: &str) -> bool {
    sim.world.meta.with_db(|db| {
        db.outbox
            .get(prompt_id)
            .is_some_and(|r| r.acked_at.is_some())
    })
}

/// The incident interleaving with the fixes: the attach lands AFTER the
/// woken Deliver op's first attempt. The mid-grace `NotFound` defers
/// WITHOUT a destructive reattach (A4); the Idle announcement recalls
/// the deferred row (A8) and wakes the deliver (A3), so the prompt is
/// delivered + acked IMMEDIATELY at the attach — not after the row's
/// deferral cadence, and not after 40+ seconds of accumulated backoff.
#[test]
fn regression_2026_07_31_attach_race_delivers_on_the_attach_signal() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(0x0108, Profile::Calm);
        // Enable the real A3 announcement path (scenario opt-in; the
        // swarm completes attaches silently — see AttachPlane::announce).
        sim.world.host_world.set_attach_announce(true);
        // Delay the dial past the woken deliver's first attempt: the
        // attach-delayed-by-N fault, N = 500 ms.
        sim.world
            .host_world
            .set_attach_delay(Duration::from_millis(500));
        let t0 = engram_core::traits::Clock::now_utc(&*sim.world.clock);
        let prompt_id = "ttft-race-prompt";
        let (sid, sandbox, _host) = create_with_prompt(&mut sim, prompt_id).await;

        // The dial is in flight; the boot's start_agent is the ONLY
        // nudge so far.
        assert!(!sim.world.host_world.harness_attached(sandbox));
        let dial0 = sim
            .world
            .host_world
            .dial_serial(sandbox)
            .expect("dial in flight after start_agent");
        assert_eq!(sim.world.host_world.attach_nudges(sandbox), 1);

        // The woken Deliver op races the attach and loses (`NotFound`).
        // A4: inside the grace this is a fixed-cadence deferral, never
        // the destructive reattach.
        sim.execute(Step::Driver(0, DriverKind::SessionOps)).await;
        let (state, error, _) = deliver_op(&sim, sid).expect("deliver op exists");
        assert_eq!(state, OpState::Queued, "deliver defers, not fails");
        let error = error.expect("deferral reason recorded");
        assert!(
            error.contains("attach grace"),
            "mid-grace NotFound must take the A4 grace arm, got: {error}"
        );
        assert_eq!(
            sim.world.host_world.dial_serial(sandbox),
            Some(dial0),
            "the in-flight dial must NOT be dropped/restarted inside the grace"
        );
        assert_eq!(
            sim.world.host_world.attach_nudges(sandbox),
            1,
            "no destructive reattach (start_agent) inside the grace"
        );
        assert!(!prompt_acked(&sim, prompt_id));

        // 600 ms later the dial completes; the pump announces Idle
        // through the real ingestion route. A3+A8: the sink RECALLS the
        // session's waiting rows (`outbox_make_due`, before the op
        // wake) and wakes the backed-off deliver — BOTH the row and the
        // op are due NOW. Pre-A8 only the op was woken and delivery
        // still waited out the row's 1 s deferral cadence; the recall
        // removes that last wait.
        sim.execute(Step::AdvanceTime(Duration::from_millis(600)))
            .await;
        assert!(sim.world.host_world.harness_attached(sandbox));
        let now = engram_core::traits::Clock::now_utc(&*sim.world.clock);
        let (_, _, op_not_before) = deliver_op(&sim, sid).expect("deliver op exists");
        assert!(
            op_not_before.expect("queued op has not_before") <= now,
            "the attach signal must wake the deliver op NOW"
        );
        let row_not_before = sim
            .world
            .meta
            .with_db(|db| db.outbox.get(prompt_id).map(|r| r.not_before))
            .expect("outbox row");
        assert!(
            row_not_before <= now,
            "A8: the attach signal must recall the deferred row NOW — no cadence wait"
        );

        // The next executor pass forwards immediately and the harness
        // echo acks — no further time advance needed.
        sim.execute(Step::Driver(0, DriverKind::SessionOps)).await;
        assert!(
            prompt_acked(&sim, prompt_id),
            "the prompt must deliver + ack right after the attach"
        );
        let elapsed =
            engram_core::traits::Clock::now_utc(&*sim.world.clock).signed_duration_since(t0);
        assert!(
            elapsed <= chrono::Duration::seconds(1),
            "TTFT must be attach-bounded (~0.6 s dial) with zero cadence tax, got {}ms",
            elapsed.num_milliseconds()
        );
        assert_eq!(
            sim.world.host_world.attach_nudges(sandbox),
            1,
            "the whole delivery ran without a single destructive reattach"
        );
        // The world is at rest and every oracle holds.
        invariants::check_quiescence(&sim.world).expect("quiescent after delivery");
    });
}

/// The swallowed-attach variant: the dial's attach frame is black-holed
/// (the ADR 0108 vsock RX-gate class) until a reattach nudge restarts
/// it. Inside the grace the deliver defers without nudging; past the
/// grace the destructive remedy is correct — sim DevVm sessions resolve
/// no AgentSpec (`reattach_harness_in_place` returns `Ok(false)`), so
/// the scenario issues the SIGUSR1-equivalent nudge directly and
/// asserts it DROPS the in-flight dial and restarts it, after which
/// delivery converges within the TTFT oracle bound of the heal.
#[test]
fn regression_2026_07_31_swallowed_attach_converges_after_nudge() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(0x0109, Profile::Calm);
        sim.world.host_world.set_attach_announce(true);
        for host in sim.world.host_ids.clone() {
            sim.world.host_world.set_attach_swallowed(host, true);
        }
        let prompt_id = "ttft-swallowed-prompt";
        let (sid, sandbox, host) = create_with_prompt(&mut sim, prompt_id).await;
        let dial0 = sim
            .world
            .host_world
            .dial_serial(sandbox)
            .expect("swallowed dial in flight");

        // Ride out the 10 s grace on the fixed 1 s cadence. The dial
        // stays black-holed; the deliver keeps deferring; no reattach
        // fires INSIDE the grace.
        for _ in 0..10 {
            sim.execute(Step::AdvanceTime(Duration::from_secs(1))).await;
            sim.execute(Step::Driver(0, DriverKind::SessionOps)).await;
        }
        assert!(!sim.world.host_world.harness_attached(sandbox));
        assert_eq!(
            sim.world.host_world.dial_serial(sandbox),
            Some(dial0),
            "the swallowed dial is untouched through the grace window"
        );
        assert_eq!(sim.world.host_world.attach_nudges(sandbox), 1);
        assert!(!prompt_acked(&sim, prompt_id));
        let (state, _, _) = deliver_op(&sim, sid).expect("deliver op exists");
        assert_eq!(
            state,
            OpState::Queued,
            "delivery keeps retrying, never fails"
        );

        // Past the grace the coordinator fires the destructive reattach.
        // (In the sim, DevVm sessions make that a no-op — the harness
        // re-bake for agent-mode sim sessions is the noted follow-up —
        // so the scenario issues the SIGUSR1-equivalent itself.) Heal
        // the fault, then nudge: the in-flight dial is DROPPED and a
        // fresh, un-swallowed dial replaces it.
        for h in sim.world.host_ids.clone() {
            sim.world.host_world.set_attach_swallowed(h, false);
        }
        sim.world.host_world.nudge_attach(host, sandbox);
        let dial1 = sim
            .world
            .host_world
            .dial_serial(sandbox)
            .expect("fresh dial after the nudge");
        assert_ne!(
            dial1, dial0,
            "the nudge drops the in-flight dial and restarts it"
        );
        assert_eq!(sim.world.host_world.attach_nudges(sandbox), 2);
        let healed_at = engram_core::traits::Clock::now_utc(&*sim.world.clock);

        // Delivery converges: attach completes, Idle wakes the deliver,
        // the row rides out its accumulated backoff, forward + echo ack.
        let mut acked = false;
        for _ in 0..40 {
            sim.execute(Step::AdvanceTime(Duration::from_secs(1))).await;
            sim.execute(Step::Driver(0, DriverKind::SessionOps)).await;
            sim.execute(Step::Driver(0, DriverKind::OutboxDelivery))
                .await;
            if prompt_acked(&sim, prompt_id) {
                acked = true;
                break;
            }
        }
        assert!(acked, "the swallowed-then-nudged prompt must deliver");
        assert!(sim.world.host_world.harness_attached(sandbox));
        let since_heal =
            engram_core::traits::Clock::now_utc(&*sim.world.clock).signed_duration_since(healed_at);
        assert!(
            since_heal <= chrono::Duration::seconds(30),
            "delivery must converge within the TTFT oracle bound of the nudge, took {}s",
            since_heal.num_seconds()
        );
        invariants::ttft_liveness(&sim.world).expect("TTFT oracle clean after convergence");
    });
}

/// NON-VACUITY: the TTFT liveness oracle FIRES when delivery is
/// genuinely stalled (attach black-holed forever, session Active, row
/// unacked past the bound). An oracle that can't fire is worthless.
#[test]
fn ttft_liveness_oracle_fires_on_a_stalled_delivery() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(0x010A, Profile::Calm);
        sim.world.host_world.set_attach_announce(true);
        for host in sim.world.host_ids.clone() {
            sim.world.host_world.set_attach_swallowed(host, true);
        }
        let prompt_id = "ttft-stalled-prompt";
        let (_sid, sandbox, _host) = create_with_prompt(&mut sim, prompt_id).await;
        invariants::ttft_liveness(&sim.world).expect("a young unacked row is within the bound");
        for _ in 0..35 {
            sim.execute(Step::AdvanceTime(Duration::from_secs(1))).await;
            sim.execute(Step::Driver(0, DriverKind::SessionOps)).await;
        }
        assert!(!sim.world.host_world.harness_attached(sandbox));
        let violation =
            invariants::ttft_liveness(&sim.world).expect_err("the stalled delivery must fire");
        assert_eq!(violation.invariant, "ttft-liveness");
    });
}

/// NON-VACUITY: the attach-disagreement oracle FIRES when coordinator-
/// Active + world-sandbox-running + no-harness-attached persists past
/// the bound, and stays quiet while the disagreement is young.
#[test]
fn attach_disagreement_oracle_fires_on_a_black_holed_attach() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(0x010B, Profile::Calm);
        sim.world.host_world.set_attach_announce(true);
        for host in sim.world.host_ids.clone() {
            sim.world.host_world.set_attach_swallowed(host, true);
        }
        sim.execute(Step::HostHeartbeats).await;
        sim.execute(Step::CreateSession).await;
        let sandbox = sim
            .world
            .meta
            .with_db(|db| {
                db.sessions
                    .values()
                    .find(|r| r.session.status == SessionState::Active)
                    .and_then(|r| r.session.sandbox_id)
            })
            .expect("one Active session with a bound sandbox");
        assert!(!sim.world.host_world.harness_attached(sandbox));

        let mut oracles = invariants::Oracles::default();
        oracles
            .check_step(&sim.world)
            .expect("a young attach disagreement is a legal transient");
        sim.execute(Step::AdvanceTime(Duration::from_secs(31)))
            .await;
        let violation = oracles
            .check_step(&sim.world)
            .expect_err("a 31 s attach disagreement must fire");
        assert_eq!(violation.invariant, "attach-disagreement");
    });
}

/// The prod-7eddce62 fourth stall shape (ADR 0108 A8), pinned. In prod:
/// a 27-minute rung-2 park killed the harness vsock link while the hub
/// still advertised the handle; the un-park took 120 ms; `send_prompt`
/// returned Ok into the dead socket; the row waited out the full 30 s
/// ACK_TIMEOUT. The world models no VM pause, so the severance is
/// injected directly with the Stale fault (the documented entry point —
/// destroy/crash severances stay immediate). Under test, both A8 call
/// sites on this branch:
///   - the heartbeat disagreement is a REPAIR: `harness_desync::
///     run_once` (driven by the sim's heartbeat step, mirroring the
///     real handler) recalls the waiting row and enqueues the Deliver
///     op the moment the reap opens the disagreement;
///   - fresh attach evidence recalls waiting rows at the state.rs call
///     site: the recovery Idle announcement pulls the failure-deferred
///     row due ahead of its backoff (`outbox_make_due` before the A3
///     op wake).
///
/// The stall must be bounded by the reap window + the heartbeat
/// cadence, never by ACK_TIMEOUT.
#[test]
fn regression_7eddce62_stale_handle_heartbeat_repair_recalls_the_row() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(0x010C, Profile::Calm);
        sim.world.host_world.set_attach_announce(true);
        // The hub must notice the dead link within 5 s of severance.
        sim.world
            .host_world
            .set_attach_reap_window(Duration::from_secs(5));
        let (sid, sandbox, host) = create_active_attached(&mut sim).await;
        let state = sim.world.replicas[0]
            .state
            .clone()
            .expect("replica 0 is up");

        // The long-pause severance. The hub still advertises the
        // handle: the heartbeat `attached` derivation must carry it,
        // so NO disagreement exists yet (as in prod — the alarm fired
        // only after the hub noticed).
        assert!(sim.world.host_world.sever_attach_stale(sandbox));
        assert!(sim.world.host_world.harness_stale(sandbox));
        assert!(
            sim.world.host_world.harness_attached(sandbox),
            "a Stale handle IS attached in the hub's view"
        );

        // The prompt arrives; the prompt path's direct Deliver op
        // forwards Ok into the dead socket.
        let prompt_id = "ttft-stale-prompt";
        let t_prompt = engram_core::traits::Clock::now_utc(&*sim.world.clock);
        assert!(workload::api_prompt(&state, sid, prompt_id).await);
        workload::drain_detached().await;
        let row = outbox_row(&sim, prompt_id);
        assert!(
            row.delivered_at.is_some(),
            "the forward returned Ok — the row is marked delivered"
        );
        assert_eq!(row.attempts, 1, "delivery attempts bumped exactly once");
        assert!(row.acked_at.is_none());
        // NON-VACUITY (the pre-repair shape): the row's only self-
        // recovery is its ACK deadline, a full ACK_TIMEOUT (30 s) out —
        // exactly the ttft-liveness bound. Without the A8 recall no
        // redelivery can run before the oracle fires (proven end-to-end
        // by ttft_liveness_oracle_fires_on_a_stale_handle_without_the_repair).
        assert_eq!(
            row.not_before,
            t_prompt + chrono::Duration::seconds(30),
            "the delivered row waits a full ACK_TIMEOUT"
        );

        // A dead socket produces no run_started echo, and no Idle
        // announcement occurs while the handle is Stale.
        sim.execute(Step::AdvanceTime(Duration::from_secs(1))).await;
        assert!(!prompt_acked(&sim, prompt_id), "no echo from a dead link");
        assert!(sim.world.host_world.harness_stale(sandbox));

        // The reap fires within its window: the hub drops the handle
        // and the running-but-unattached disagreement opens.
        sim.execute(Step::AdvanceTime(Duration::from_secs(4))).await;
        assert!(
            !sim.world.host_world.harness_attached(sandbox),
            "the hub reaped the stale handle at its window"
        );

        // The next heartbeat runs the A8 repair (`harness_desync::
        // run_once` right after `touch_host_heartbeat`): the row is
        // recalled off its ACK deadline and a Deliver op is enqueued
        // and driven. The forward answers NotFound on the reaped
        // handle, so the op retries on the short failure backoff — the
        // normal recovery path has begun.
        sim.execute(Step::HostHeartbeats).await;
        let row = outbox_row(&sim, prompt_id);
        assert!(row.acked_at.is_none());
        assert!(
            row.not_before < t_prompt + chrono::Duration::seconds(30),
            "the repair recalled the row off its 30 s ACK deadline"
        );
        let (op_state, _, _) = deliver_op(&sim, sid).expect("the repair enqueued a Deliver op");
        assert_eq!(
            op_state,
            OpState::Queued,
            "the deliver retries behind the NotFound on the reaped handle"
        );

        // Recovery: the destructive remedy re-establishes the harness.
        // (Sim DevVm sessions resolve no AgentSpec —
        // `reattach_harness_in_place` returns Ok(false) — so issue the
        // SIGUSR1-equivalent nudge directly, the swallowed-attach
        // scenario's documented pattern.) Dial → Attached → Idle
        // announcement.
        sim.world.host_world.nudge_attach(host, sandbox);
        sim.execute(Step::AdvanceTime(Duration::from_millis(300)))
            .await;
        assert!(sim.world.host_world.harness_attached(sandbox));

        // A8 at the STATE.RS call site (distinct from the heartbeat
        // one): the Idle announcement recalled the failure-deferred row
        // ahead of its backoff — a delivered-but-unacked row never
        // waits out a deadline once the harness provably attached.
        let now = engram_core::traits::Clock::now_utc(&*sim.world.clock);
        let row = outbox_row(&sim, prompt_id);
        assert!(row.acked_at.is_none());
        assert!(
            row.not_before <= now,
            "the attach signal made the row due NOW, ahead of its backoff"
        );

        // The woken deliver forwards on the live link; the pump's echo
        // acks the row within the same step.
        sim.execute(Step::Driver(0, DriverKind::SessionOps)).await;
        assert!(
            prompt_acked(&sim, prompt_id),
            "delivered + acked on the re-established link"
        );

        // Total stall: 1 s (echo check) + 4 s (reap window remainder)
        // + heartbeat + 0.3 s (re-dial) — bounded by the reap window
        // plus the heartbeat cadence, NOT by the 30 s ACK_TIMEOUT.
        let elapsed =
            engram_core::traits::Clock::now_utc(&*sim.world.clock).signed_duration_since(t_prompt);
        assert!(
            elapsed <= chrono::Duration::seconds(10),
            "stall bounded by reap window + heartbeat cadence, got {}ms",
            elapsed.num_milliseconds()
        );
        invariants::check_quiescence(&sim.world)
            .expect("quiescent after the repair-driven recovery");
    });
}

/// NON-VACUITY for the A8 repair: the SAME severance with the repair
/// never driven — no HostHeartbeats step runs, and the heartbeat step
/// is the swarm's only `harness_desync::run_once` call site — leaves
/// the delivered row parked on its full ACK_TIMEOUT deadline, and the
/// ttft-liveness oracle fires at its 30 s bound: the incident stall,
/// reproduced. No test-only switch in production code is needed: a
/// heartbeat-free timeline IS the repair-neutralized world.
#[test]
fn ttft_liveness_oracle_fires_on_a_stale_handle_without_the_repair() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(0x010D, Profile::Calm);
        sim.world.host_world.set_attach_announce(true);
        sim.world
            .host_world
            .set_attach_reap_window(Duration::from_secs(5));
        let (sid, sandbox, _host) = create_active_attached(&mut sim).await;
        let state = sim.world.replicas[0]
            .state
            .clone()
            .expect("replica 0 is up");
        assert!(sim.world.host_world.sever_attach_stale(sandbox));
        let prompt_id = "ttft-stale-unrepaired-prompt";
        assert!(workload::api_prompt(&state, sid, prompt_id).await);
        workload::drain_detached().await;
        assert_eq!(outbox_row(&sim, prompt_id).attempts, 1);

        // 29 virtual seconds of a live but heartbeat-free coordinator:
        // the reap fires at 5 s, but with no heartbeat there is no
        // repair — nothing recalls the row before its ACK deadline.
        for _ in 0..29 {
            sim.execute(Step::AdvanceTime(Duration::from_secs(1))).await;
            sim.execute(Step::Driver(0, DriverKind::SessionOps)).await;
        }
        assert!(
            !sim.world.host_world.harness_attached(sandbox),
            "reaped and never re-established"
        );
        invariants::ttft_liveness(&sim.world).expect("29 s in, the row is still within the bound");
        let row = outbox_row(&sim, prompt_id);
        assert_eq!(row.attempts, 1, "no redelivery beat the ACK deadline");
        assert!(row.acked_at.is_none());

        sim.execute(Step::AdvanceTime(Duration::from_secs(2))).await;
        let violation = invariants::ttft_liveness(&sim.world)
            .expect_err("31 s of Active-with-unacked-prompt is the incident stall");
        assert_eq!(violation.invariant, "ttft-liveness");
    });
}
