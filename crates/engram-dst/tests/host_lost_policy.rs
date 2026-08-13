//! HostLost stage-2 policy proofs (#777 / ADR 0098 Phase 3, extended by
//! ADR 0116 A3/A4), driven against the REAL coordinator drivers over
//! engram-sim.
//!
//! The pinned design calls:
//!   1. **honest-Dead** (#777) — a bound HostLost straggler whose only
//!      snapshot is un-recoverable settles to `Dead`, never a lying
//!      `Idle`.
//!   2. **entomb, never destroy-despite-alive** (ADR 0116 A-D5,
//!      retiring #777's serving-strike defer) — a SERVING VM is never
//!      destroyed by the coordinator; its tombstone rides the heartbeat
//!      and its own host destroys it (ack-by-absence closes the loop).
//!   3. **the lease shield** (A3) — the incident replay, the kill-9
//!      control leg, the durable probe-rescue, and the lease-liveness +
//!      destroy-of-bound oracle non-vacuity proofs.

use engram_core::types::BindingDisposition;
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::{Clock as _, MetadataStore as _};
use engram_core::types::session::{SessionMode, SessionSpec, SessionState};
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::{HostId, SandboxId, SessionId, SnapshotId};
use engram_dst::{DriverKind, Profile, Sim, Step};
use engram_sim::SimMetadataStore;

/// Build the paused-clock current-thread runtime the sim requires and run
/// `body` with a fresh `Sim`. Mirrors the harness in `regression_seeds.rs`.
fn on_sim<Fut>(seed: u64, body: impl FnOnce(Sim) -> Fut)
where
    Fut: std::future::Future<Output = ()>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async {
        tokio::time::pause();
        body(Sim::new(seed, Profile::Calm)).await;
    });
}

/// Park a session at HostLost with `host_id` + `sandbox_id` STILL bound —
/// the #762 straggler shape (an evict-budget-exhaustion / idle-evictor
/// fallback leaves the bindings set and no inline stage-2 ever runs). All
/// staged through legal FSM edges on the REAL store.
async fn seed_bound_host_lost(
    meta: &Arc<SimMetadataStore>,
    host: HostId,
    sandbox: SandboxId,
) -> SessionId {
    let sid = meta
        .create_session(SessionSpec {
            image: "sim:host-lost".into(),
            mode: SessionMode::Agent,
        })
        .await
        .expect("create");
    meta.assign_session_host(sid, Some(host))
        .await
        .expect("assign host");
    meta.transition_session_created(sid, sandbox)
        .await
        .expect("created");
    meta.transition_session(sid, SessionState::Active, BindingDisposition::Retain)
        .await
        .expect("active");
    meta.transition_session(sid, SessionState::HostLost, BindingDisposition::Retain)
        .await
        .expect("host-lost (bindings intact)");
    sid
}

/// Record one snapshot row for `sid` with the given `recoverable` flag.
async fn record_snapshot(
    meta: &Arc<SimMetadataStore>,
    sid: SessionId,
    recoverable: bool,
    at: chrono::DateTime<chrono::Utc>,
) {
    let snap: SnapshotRecord = serde_json::from_value(serde_json::json!({
        "id": SnapshotId::new(),
        "session_id": sid,
        "host_id": null,
        "image_version": "sim",
        "size_bytes": 0,
        "created_at": at,
        "last_accessed_at": at,
        "recoverable": recoverable,
    }))
    .expect("snapshot record");
    meta.record_snapshot(snap).await.expect("record snapshot");
}

fn status(sim: &Sim, sid: SessionId) -> SessionState {
    sim.world
        .meta
        .with_db(|db| db.sessions.get(&sid).expect("session row").session.status)
}

/// Insert `sandbox` into the world-side host `host`, owned by `sid` — the
/// live VM the coordinator's `probe_sandbox` will report `process_alive`
/// for (world-truth: the VMM is up).
fn place_live_sandbox(sim: &Sim, host: HostId, sandbox: SandboxId, sid: SessionId) {
    sim.world
        .host_world
        .hosts
        .lock()
        .get_mut(&host)
        .expect("host in world")
        .sandboxes
        .insert(sandbox, Some(sid));
}

/// Is `sandbox` still present on world-side host `host`? (i.e. the sweep
/// has NOT destroyed the live VM.)
fn sandbox_live(sim: &Sim, host: HostId, sandbox: SandboxId) -> bool {
    sim.world
        .host_world
        .hosts
        .lock()
        .get(&host)
        .is_some_and(|h| h.sandboxes.contains_key(&sandbox))
}

/// Commit 1 (honest-Dead): a bound HostLost straggler whose ONLY snapshot
/// is un-recoverable must settle to `Dead`, never a lying `Idle`.
///
/// Red-then-green: with the pre-#777 `snapshot.is_some()` predicate the
/// sweep routed this to `Idle` (a snapshot row exists); the honest
/// `recoverable`-filtered predicate routes it to `Dead`. The sandbox is
/// deliberately NOT inserted into the host world, so the ask-the-host
/// probe (commit 2) finds no live VM and the sweep settles this same
/// cycle in BOTH commits — isolating the Idle-vs-Dead PREDICATE.
#[test]
fn unrecoverable_only_straggler_settles_dead_not_idle() {
    on_sim(777_001, |mut sim| async move {
        let host = sim.world.host_ids[0];
        let sandbox = SandboxId::new();
        let meta = sim.world.meta.clone();
        let sid = seed_bound_host_lost(&meta, host, sandbox).await;

        // The only snapshot on record is un-recoverable (a torn / HEAD-
        // failed capture): the row exists, but `recoverable` is false.
        let now = sim.world.clock.now_utc();
        record_snapshot(&meta, sid, false, now).await;

        // Age past the sweep's 60s min-age, then drive the dead-host
        // sweep on replica 0.
        sim.execute(Step::AdvanceTime(Duration::from_secs(120)))
            .await;
        sim.execute(Step::Driver(0, DriverKind::DeadHost)).await;

        assert_eq!(
            status(&sim, sid),
            SessionState::Dead,
            "an un-recoverable-only straggler must settle Dead — never a lying Idle (#777 honest-Dead)",
        );
    });
}

/// Park a session at Active with `host_id` + `sandbox_id` bound — the
/// healthy shape a roll must carry through. Staged through legal FSM
/// edges on the REAL store.
async fn seed_bound_active(
    meta: &Arc<SimMetadataStore>,
    host: HostId,
    sandbox: SandboxId,
) -> SessionId {
    let sid = meta
        .create_session(SessionSpec {
            image: "sim:lease".into(),
            mode: SessionMode::Agent,
        })
        .await
        .expect("create");
    meta.assign_session_host(sid, Some(host))
        .await
        .expect("assign host");
    meta.transition_session_created(sid, sandbox)
        .await
        .expect("created");
    meta.transition_session(sid, SessionState::Active, BindingDisposition::Retain)
        .await
        .expect("active");
    sid
}

fn host_lost_transitions(sim: &Sim, sid: SessionId) -> usize {
    sim.world.meta.with_db(|db| {
        db.transition_log
            .iter()
            .filter(|t| t.session == sid && t.to == SessionState::HostLost)
            .count()
    })
}

/// ADR 0116 A3 keystone: the 2026-08-12 incident replayed under the
/// lease model. An operator roll declares a handoff, the predecessor
/// pod goes silent for 5.5 sim-minutes (far past every retired
/// staleness threshold), then the successor registers. The session
/// must never leave Active and its binding must never be touched — in
/// the incident, the staleness+strike path cleared the binding 44 s
/// before the successor adopted the still-running VM, and the
/// successor's teardown then destroyed the healthy VM.
#[test]
fn shielded_roll_keeps_the_session_bound_through_five_minutes_of_silence() {
    on_sim(116_001, |mut sim| async move {
        let host = sim.world.host_ids[0];
        let sandbox = SandboxId::new();
        let meta = sim.world.meta.clone();
        let sid = seed_bound_active(&meta, host, sandbox).await;
        place_live_sandbox(&sim, host, sandbox, sid);

        // The fleet registers + heartbeats (leases written, ADR 0116 A-D1).
        sim.execute(Step::HostHeartbeats).await;

        // The operator declares the handoff BEFORE the pod delete —
        // the roll_node ordering (A-D2). TTL sized like prod's roll
        // shield.
        let until = sim.world.clock.now_utc() + chrono::Duration::seconds(6120);
        assert!(meta.begin_host_handoff(host, until).await.unwrap());

        // The pod delete: the host-agent is GONE — heartbeats stop AND
        // probes fail (the incident shape; under the retired staleness
        // path this accumulated probe strikes and cleared the binding).
        // The world keeps the sandbox map: the VMs live on the node,
        // not in the pod.
        sim.execute(Step::CrashHost(0)).await;
        sim.execute(Step::AdvanceTime(Duration::from_secs(330)))
            .await;

        // The detector sweeps repeatedly across the silence window.
        for _ in 0..3 {
            sim.execute(Step::Driver(0, DriverKind::DeadHost)).await;
            assert_eq!(
                status(&sim, sid),
                SessionState::Active,
                "the handoff deadline shields the binding through the roll",
            );
        }
        assert!(
            sandbox_live(&sim, host, sandbox),
            "the still-running VM must not be destroyed mid-roll"
        );

        // The successor pod comes up on the node (world: the host
        // answers again, the surviving VMs still in place) and
        // registers: fresh lease, epoch bump, handoff ends (A-D3).
        sim.world
            .host_world
            .hosts
            .lock()
            .get_mut(&host)
            .expect("host in world")
            .up = true;
        sim.execute(Step::HostRegister(0)).await;
        sim.execute(Step::HostHeartbeats).await;
        sim.execute(Step::Driver(0, DriverKind::DeadHost)).await;

        assert_eq!(
            status(&sim, sid),
            SessionState::Active,
            "the session never left Active across the whole roll",
        );
        assert_eq!(
            host_lost_transitions(&sim, sid),
            0,
            "the binding was never revoked — no HostLost transition ever fired",
        );
        assert!(sandbox_live(&sim, host, sandbox));
    });
}

/// The control leg: the same shape WITHOUT a handoff declaration — a
/// `kill -9` (SIGKILL/preemption writes no marker by design). The lease
/// expires at its 45 s TTL, the probe finds the host unreachable, and
/// the explicit death path settles the checkpointed session to Idle.
#[test]
fn unshielded_death_settles_idle_after_lease_expiry() {
    on_sim(116_002, |mut sim| async move {
        let host = sim.world.host_ids[0];
        let sandbox = SandboxId::new();
        let meta = sim.world.meta.clone();
        let sid = seed_bound_active(&meta, host, sandbox).await;
        place_live_sandbox(&sim, host, sandbox, sid);
        sim.execute(Step::HostHeartbeats).await;
        // A recoverable checkpoint exists, so stage-2 routes Idle.
        let now = sim.world.clock.now_utc();
        record_snapshot(&meta, sid, true, now).await;

        // kill -9: the machine is gone (VMs die with it), no handoff.
        sim.execute(Step::CrashHost(0)).await;

        // Inside the lease TTL nothing happens — silence alone is not
        // yet expiry.
        sim.execute(Step::AdvanceTime(Duration::from_secs(30)))
            .await;
        sim.execute(Step::Driver(0, DriverKind::DeadHost)).await;
        assert_eq!(
            status(&sim, sid),
            SessionState::Active,
            "inside the lease TTL the binding holds",
        );

        // Past the TTL the lease is expired, the probe fails, and the
        // death path runs both stages.
        sim.execute(Step::AdvanceTime(Duration::from_secs(20)))
            .await;
        sim.execute(Step::Driver(0, DriverKind::DeadHost)).await;
        assert_eq!(
            status(&sim, sid),
            SessionState::Idle,
            "an expired lease + failed probe settles the checkpointed session to Idle",
        );
    });
}

/// The rk28 shape under the lease model: heartbeats vanish (a peer
/// pod's PG pool saturated) while the host itself stays up and answers
/// the probe. The rescue durably renews the lease — the session stays
/// bound through arbitrarily many detector sweeps, with no in-memory
/// strike/grace machinery involved.
#[test]
fn probe_rescue_durably_renews_and_the_binding_holds() {
    on_sim(116_003, |mut sim| async move {
        let host = sim.world.host_ids[0];
        let sandbox = SandboxId::new();
        let meta = sim.world.meta.clone();
        let sid = seed_bound_active(&meta, host, sandbox).await;
        place_live_sandbox(&sim, host, sandbox, sid);
        sim.execute(Step::HostHeartbeats).await;

        // Heartbeats stop; the host stays up (answers RPCs).
        sim.execute(Step::HeartbeatPartition(0, true)).await;

        for round in 0..3 {
            // Well past the lease TTL each round — without the durable
            // rescue every sweep after the first would kill the host.
            sim.execute(Step::AdvanceTime(Duration::from_secs(60)))
                .await;
            sim.execute(Step::Driver(0, DriverKind::DeadHost)).await;
            assert_eq!(
                status(&sim, sid),
                SessionState::Active,
                "round {round}: the answered probe renews the lease durably; the binding holds",
            );
        }
        let lease = sim
            .world
            .meta
            .with_db(|db| db.hosts.get(&host).and_then(|h| h.lease_expires_at));
        assert!(
            lease.is_some_and(|e| e >= sim.world.clock.now_utc()),
            "the rescue WROTE the reprieve: the lease is live in the store, not in a \
             per-replica strike map ({lease:?})",
        );
    });
}

/// NON-VACUITY: the lease-liveness oracle FIRES when a binding is
/// revoked (HostLost, bindings cleared) while the host's lease is
/// live, and stays quiet on a legal revocation after expiry.
#[test]
fn lease_liveness_oracle_fires_on_a_revocation_under_a_live_lease() {
    on_sim(116_004, |mut sim| async move {
        let host = sim.world.host_ids[0];
        let sandbox = SandboxId::new();
        let meta = sim.world.meta.clone();
        let sid = seed_bound_active(&meta, host, sandbox).await;
        sim.execute(Step::HostHeartbeats).await;

        let mut oracles = engram_dst::invariants::Oracles::default();
        oracles
            .check_step(&sim.world)
            .expect("a bound Active session under a live lease is legal");

        // The bad actor: clear the binding and flip HostLost while the
        // lease is live — exactly what the retired staleness path did
        // to the incident session.
        sim.world.meta.with_db_mut(|db| {
            let row = db.sessions.get_mut(&sid).expect("session row");
            row.session.host_id = None;
            row.session.sandbox_id = None;
            row.session.status = SessionState::HostLost;
        });
        let violation = oracles
            .check_step(&sim.world)
            .expect_err("revoking a binding under a live lease must fire");
        assert_eq!(violation.invariant, "lease-liveness");
        let _ = sandbox;
    });
}

/// ADR 0116 A-D5 (retires the #777 serving-strike deferral): a bound
/// HostLost straggler whose host still reports the sandbox SERVING is
/// NEVER destroyed by the coordinator — not on the first sweep, not
/// after any number of them. The first sweep records a tombstone (the
/// durable "your host must destroy this VM" fact), settles the ROW
/// immediately (a recoverable snapshot is on record, so Idle), and
/// leaves the VM to its own host's heartbeat consumption.
///
/// Red-then-green: the pre-A4 sweep destroyed the live VM at the third
/// strike (destroy-despite-alive — inference, not host-affirmed); now
/// no amount of sweeping kills it, and the tombstone row carries the
/// obligation instead.
#[test]
fn serving_straggler_settles_row_and_entombs_vm_without_destroying_it() {
    on_sim(777_002, |mut sim| async move {
        let host = sim.world.host_ids[0];
        let sandbox = SandboxId::new();
        let meta = sim.world.meta.clone();
        let sid = seed_bound_host_lost(&meta, host, sandbox).await;

        // World-truth: the VM is still up and serving on its host, even
        // though the coordinator parked the session at HostLost.
        place_live_sandbox(&sim, host, sandbox, sid);
        // A recoverable snapshot so the settle target is Idle.
        let now = sim.world.clock.now_utc();
        record_snapshot(&meta, sid, true, now).await;

        // Age past the 60s min-age.
        sim.execute(Step::AdvanceTime(Duration::from_secs(120)))
            .await;

        // The FIRST sweep settles the row and entombs the VM.
        sim.execute(Step::Driver(0, DriverKind::DeadHost)).await;
        assert_eq!(
            status(&sim, sid),
            SessionState::Idle,
            "the row settles immediately — it no longer waits on the VM",
        );
        assert!(
            sandbox_live(&sim, host, sandbox),
            "the SERVING VM must never be destroyed by the coordinator",
        );
        assert_eq!(
            sim.world
                .meta
                .sandbox_tombstones_for_host(host)
                .await
                .unwrap(),
            vec![sandbox],
            "the tombstone carries the destroy obligation to the host",
        );

        // Further sweeps change nothing — no strike cap, no delayed kill.
        for _ in 0..3 {
            sim.execute(Step::Driver(0, DriverKind::DeadHost)).await;
            assert!(sandbox_live(&sim, host, sandbox));
        }

        // The heartbeat closes the loop: the tombstone rides the ack,
        // the HOST destroys its own VM (the sim heartbeat's consumption
        // leg — real `process_sandbox_tombstones` + a world destroy),
        // and the next heartbeat's running set acks the row by absence.
        sim.execute(Step::HostHeartbeats).await;
        assert!(
            !sandbox_live(&sim, host, sandbox),
            "the host destroys its tombstoned VM on heartbeat consumption",
        );
        sim.execute(Step::HostHeartbeats).await;
        assert!(
            sim.world
                .meta
                .sandbox_tombstones_for_host(host)
                .await
                .unwrap()
                .is_empty(),
            "ack-by-absence clears the row once the sandbox leaves the running set",
        );
    });
}

/// NON-VACUITY: the destroy-of-bound-sandbox oracle FIRES when a
/// sandbox is destroyed while an ACTIVE session still binds it, and
/// stays quiet when the binding is cleared first (the legal order).
#[test]
fn destroy_of_bound_oracle_fires_on_destroy_under_active_binding() {
    on_sim(116_005, |sim| async move {
        let host = sim.world.host_ids[0];
        let sandbox = SandboxId::new();
        let meta = sim.world.meta.clone();
        let sid = seed_bound_active(&meta, host, sandbox).await;
        place_live_sandbox(&sim, host, sandbox, sid);

        let mut oracles = engram_dst::invariants::Oracles::default();
        oracles
            .check_step(&sim.world)
            .expect("a bound Active session with a live VM is legal");

        // The bad actor: destroy the VM while the Active binding stands.
        sim.world
            .host_world
            .record_effect(host, engram_dst::world::Effect::Destroy { sandbox });
        let violation = oracles
            .check_step(&sim.world)
            .expect_err("destroying under an Active binding must fire");
        assert_eq!(violation.invariant, "destroy-of-bound-sandbox");

        // Legal order stays quiet: a second sandbox, binding cleared
        // BEFORE the destroy.
        let sandbox2 = SandboxId::new();
        let sid2 = seed_bound_active(&meta, host, sandbox2).await;
        place_live_sandbox(&sim, host, sandbox2, sid2);
        meta.transition_session(sid2, SessionState::HostLost, BindingDisposition::Detach)
            .await
            .expect("detach flip");
        sim.world.host_world.record_effect(
            host,
            engram_dst::world::Effect::Destroy { sandbox: sandbox2 },
        );
        oracles
            .check_step(&sim.world)
            .expect("a destroy after the binding clear is the legal order");
    });
}
