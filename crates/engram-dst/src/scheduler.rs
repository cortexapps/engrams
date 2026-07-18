//! The seeded step scheduler (ADR 0098 D5).
//!
//! One current-thread tokio runtime with PAUSED time; each step's future
//! runs to completion before the next pick, so tokio's scheduler has
//! almost no freedom and a seed replays exactly (per-commit — the
//! lockfile pins tokio; cross-version replay is not promised).

use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::metadata::{CreateDisposition, SessionCreateWriteSet};
use engram_core::types::session::{SessionMode, SessionSpec};
use engram_core::types::session_op::{EnqueueOutcome, OpKind};
use engram_core::{HostId, SessionId};
use rand::prelude::*;
use rand::Rng;
use rand_chacha::ChaCha8Rng;

use crate::invariants;
use crate::world::SimWorld;

/// The one image every sim session boots. Seeded (enabled + base
/// snapshot) at world construction so create_boot's prepare leg
/// resolves it exactly like production.
pub const SIM_IMAGE: &str = "sim:warm-image";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// No faults — pure workload + drivers. The liveness baseline.
    Calm,
    /// Host crashes/restarts + PG outage windows.
    Chaos,
}

/// Every real driver the scheduler can step. The `driver_coverage`
/// test in tests/ asserts this list tracks the coordinator's run_once
/// surface so a new driver can't be silently unsimulated.
#[derive(Clone, Copy, Debug)]
pub enum DriverKind {
    QueueScanner,
    Reconcile,
    DeadHost,
    SessionOps,
    /// session_ops' reclaim sweep: stale-op reclaim + the
    /// pending-orphan create_boot backstop (ADR 0079 findings #4/#5).
    OpReclaim,
    IdleDetector,
    IdleEvictor,
    EvacResumer,
    OutboxDelivery,
    GcSweeps,
}

const DRIVERS: [DriverKind; 10] = [
    DriverKind::QueueScanner,
    DriverKind::Reconcile,
    DriverKind::DeadHost,
    DriverKind::SessionOps,
    DriverKind::OpReclaim,
    DriverKind::IdleDetector,
    DriverKind::IdleEvictor,
    DriverKind::EvacResumer,
    DriverKind::OutboxDelivery,
    DriverKind::GcSweeps,
];

#[derive(Debug)]
pub enum Step {
    AdvanceTime(Duration),
    Driver(usize, DriverKind),
    CreateSession,
    WorkloadBurst(u8),
    HostHeartbeats,
    CrashHost(usize),
    RestartHost(usize),
    CrashReplica(usize),
    RestartReplica(usize),
    PgOutage(bool),
    /// Asymmetric: heartbeats vanish, RPCs still answer (issue #231's
    /// probe-rescue case) — toggled.
    HeartbeatPartition(usize, bool),
    /// The inverse: heartbeats land, RPC verbs HANG (G1, the #743
    /// op-wedge class) — toggled. `true` arms a stall far above every
    /// `op_deadline` (the deadline, on the tokio timer, is what unwedges
    /// the op); `false` heals it.
    RpcHang(usize, bool),
    /// Skew one replica's wall clock by the given seconds (can be
    /// negative).
    ClockSkew(usize, i64),
    /// The host-side checkpoint uploader completing a periodic
    /// checkpoint for every bound Active session on one host — WORLD
    /// behavior (in production this is the host-agent's background
    /// actor, not a coordinator driver). This is what makes host death
    /// interesting: a checkpointed session routes HostLost → Idle and
    /// feeds the recovery ladder instead of draining to Dead.
    HostCheckpoint(usize),
    /// A user resuming an Idle session: the real Resume op through the
    /// op pipeline.
    ResumeSession,
}

#[derive(Debug)]
pub struct SimReport {
    pub seed: u64,
    pub steps_run: u64,
    pub sessions_created: u64,
    /// The full step trace — the replay-diff test compares two runs of
    /// the same seed on this.
    pub trace: Vec<String>,
}

pub struct Sim {
    pub world: SimWorld,
    rng: ChaCha8Rng,
    profile: Profile,
    /// dead_host probe history, owned across sweeps like the real loop.
    probe_memory: Vec<engram_coordinator::dead_host::ProbeMemoryMap>,
    dead_host_cfg: engram_coordinator::dead_host::DeadHostConfig,
    queue_cfg: engram_coordinator::queue_scanner::QueueScannerConfig,
    idle_cfg: engram_coordinator::idle_detector::IdleDetectorConfig,
    evac_cfg: engram_coordinator::evac_resumer::EvacResumerConfig,
    report: SimReport,
    pg_out: bool,
}

impl Sim {
    pub fn new(seed: u64, profile: Profile) -> Self {
        // Forked streams: world entropy uses the seed directly (inside
        // SimWorld); the scheduler's picks use an offset stream so
        // adding a consumer doesn't shift the other.
        let rng = ChaCha8Rng::seed_from_u64(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let world = SimWorld::new(seed, 2, 3);
        world.seed_enabled_image(SIM_IMAGE);
        let replicas = world.replicas.len();
        Self {
            world,
            rng,
            profile,
            probe_memory: (0..replicas).map(|_| Default::default()).collect(),
            dead_host_cfg: engram_coordinator::dead_host::DeadHostConfig::default(),
            queue_cfg: engram_coordinator::queue_scanner::QueueScannerConfig::default(),
            idle_cfg: engram_coordinator::idle_detector::IdleDetectorConfig::default(),
            evac_cfg: engram_coordinator::evac_resumer::EvacResumerConfig::default(),
            report: SimReport {
                seed,
                steps_run: 0,
                sessions_created: 0,
                trace: Vec::new(),
            },
            pg_out: false,
        }
    }

    fn pick(&mut self) -> Step {
        let hosts = self.world.host_ids.len();
        let replicas = self.world.replicas.len();
        // Weighted pick. Weights are part of the seed contract: change
        // them and old seeds explore differently (fine — seeds pin to a
        // commit), but NEVER branch on anything non-deterministic here.
        let roll: u32 = self.rng.random_range(0..100);
        match self.profile {
            Profile::Calm => match roll {
                0..=27 => Step::AdvanceTime(Duration::from_secs(self.rng.random_range(1..30))),
                28..=55 => Step::Driver(
                    self.rng.random_range(0..replicas),
                    DRIVERS[self.rng.random_range(0..DRIVERS.len())],
                ),
                56..=69 => Step::CreateSession,
                70..=75 => Step::HostCheckpoint(self.rng.random_range(0..hosts)),
                76..=81 => Step::ResumeSession,
                _ => Step::HostHeartbeats,
            },
            // CORRECTION (this PR): D6's weight patch silently failed to
            // apply (fmt-reflowed anchor, assert-less replace) — the
            // partition/skew/burst faults had execute arms but were
            // never PICKED, so the D6/D7 swarms ran a weaker menu than
            // advertised. Weights below are the real full menu.
            Profile::Chaos => match roll {
                0..=19 => Step::AdvanceTime(Duration::from_secs(self.rng.random_range(1..30))),
                20..=42 => Step::Driver(
                    self.rng.random_range(0..replicas),
                    DRIVERS[self.rng.random_range(0..DRIVERS.len())],
                ),
                43..=49 => Step::CreateSession,
                50..=51 => Step::WorkloadBurst(self.rng.random_range(2..8)),
                52..=55 => Step::HostCheckpoint(self.rng.random_range(0..hosts)),
                56..=58 => Step::ResumeSession,
                59..=71 => Step::HostHeartbeats,
                72..=76 => Step::CrashHost(self.rng.random_range(0..hosts)),
                77..=81 => Step::RestartHost(self.rng.random_range(0..hosts)),
                82..=84 => Step::CrashReplica(self.rng.random_range(0..replicas)),
                85..=87 => Step::RestartReplica(self.rng.random_range(0..replicas)),
                88..=90 => {
                    let h = self.rng.random_range(0..hosts);
                    let on = self.rng.random_range(0..2) == 0;
                    Step::HeartbeatPartition(h, on)
                }
                91..=93 => {
                    let h = self.rng.random_range(0..hosts);
                    let on = self.rng.random_range(0..2) == 0;
                    Step::RpcHang(h, on)
                }
                94..=96 => Step::ClockSkew(
                    self.rng.random_range(0..replicas),
                    self.rng.random_range(-45..=45),
                ),
                _ => Step::PgOutage(!self.pg_out),
            },
        }
    }

    /// Execute one explicit step. `pub` so pinned regression scenarios can
    /// hand-drive an exact interleaving (the dst-host pattern) instead of
    /// relying on a swarm pick to reproduce it; the swarm path calls this
    /// with `pick()`'s output.
    pub async fn execute(&mut self, step: Step) {
        self.report.trace.push(format!("{step:?}"));
        match step {
            Step::AdvanceTime(d) => self.world.clock.advance(d).await,
            Step::Driver(r, kind) => {
                let Some(state) = self.world.replicas[r].state.clone() else {
                    return; // crashed replica: the step is a no-op tick
                };
                match kind {
                    DriverKind::QueueScanner => {
                        let _ =
                            engram_coordinator::queue_scanner::run_once(&self.queue_cfg, &state)
                                .await;
                    }
                    DriverKind::Reconcile => {
                        // The real pass: each REPORTING host's world-truth
                        // sandbox set vs the coordinator's assignments
                        // (a heartbeat-partitioned host reports nothing —
                        // reconcile only ever consumes what heartbeats
                        // carry).
                        let reports: Vec<(HostId, Vec<engram_core::SandboxId>)> = {
                            let hw = self.world.host_world.hosts.lock();
                            hw.iter()
                                .filter(|(_, h)| h.up && !h.heartbeats_partitioned)
                                .map(|(id, h)| (*id, h.sandboxes.keys().copied().collect()))
                                .collect()
                        };
                        let reconciler = engram_coordinator::reconcile::Reconciler::new(3)
                            .with_clock(state.services.clock.clone());
                        for (host_id, running) in reports {
                            let _ = reconciler
                                .reconcile_with_deps(
                                    state.services.meta.as_ref(),
                                    &state.events,
                                    &state.host_registry,
                                    host_id,
                                    &running,
                                )
                                .await;
                        }
                    }
                    DriverKind::DeadHost => {
                        let _ = engram_coordinator::dead_host::run_once(
                            &self.dead_host_cfg,
                            &state,
                            "sim-pod",
                            &mut self.probe_memory[r],
                        )
                        .await;
                    }
                    DriverKind::SessionOps => {
                        let due = state
                            .services
                            .meta
                            .op_due_sessions()
                            .await
                            .unwrap_or_default();
                        for sid in due {
                            engram_coordinator::session_ops::drive_session(&state, sid).await;
                        }
                    }
                    DriverKind::OpReclaim => {
                        // The reclaim sweep's two legs, mirrored from
                        // session_ops::spawn: re-claim stale-heartbeat
                        // running ops and resume them at their recorded
                        // step; then the pending-orphan backstop
                        // (a Placed session whose create_boot op was
                        // lost — e.g. a PG outage between the reserve
                        // and the enqueue) re-enqueues create_boot.
                        let reclaimed = state
                            .services
                            .meta
                            .op_reclaim_stale(Duration::from_secs(60), "sim-pod")
                            .await
                            .unwrap_or_default();
                        for op in reclaimed {
                            engram_coordinator::session_ops::drive_claimed(&state, op).await;
                        }
                        let orphans = state
                            .services
                            .meta
                            .orphaned_pending_sessions(Duration::from_secs(120))
                            .await
                            .unwrap_or_default();
                        for sid in orphans {
                            // Issue #722 mirror of the sweep: stale
                            // orphans fail; only fresh ones revive.
                            let stale = match state.services.meta.get_session(sid).await {
                                Ok(s) => {
                                    state.services.clock.now_utc() - s.last_active_at
                                        > chrono::Duration::minutes(10)
                                }
                                Err(_) => false,
                            };
                            if stale {
                                let _ = state
                                    .services
                                    .meta
                                    .transition_session(
                                        sid,
                                        engram_core::types::session::SessionState::Failed,
                                    )
                                    .await;
                                continue;
                            }
                            let key = format!("boot-recover:{sid}");
                            // The sim never detach-spawns mutating work; drive inside the step.
                            if let Ok(EnqueueOutcome::Claimed(op)) =
                                engram_coordinator::session_ops::enqueue_claim(
                                    &state,
                                    sid,
                                    OpKind::CreateBoot,
                                    serde_json::json!({ "recovered": true }),
                                    Some(&key),
                                )
                                .await
                            {
                                engram_coordinator::session_ops::drive_claimed(&state, op).await;
                            }
                        }
                    }
                    DriverKind::IdleDetector => {
                        let _ = engram_coordinator::idle_detector::run_once(&self.idle_cfg, &state)
                            .await;
                    }
                    DriverKind::IdleEvictor => {
                        let _ = engram_coordinator::idle_evictor::scanner_run_once(&state).await;
                    }
                    DriverKind::EvacResumer => {
                        let _ = engram_coordinator::evac_resumer::run_once(&self.evac_cfg, &state)
                            .await;
                    }
                    DriverKind::OutboxDelivery => {
                        let due = state
                            .services
                            .meta
                            .outbox_due_sessions()
                            .await
                            .unwrap_or_default();
                        for sid in due {
                            engram_coordinator::outbox_delivery::enqueue_deliver_op(&state, sid)
                                .await;
                        }
                    }
                    DriverKind::GcSweeps => {
                        let cfg = engram_coordinator::chunk_gc::ChunkGcConfig::from_env();
                        let _ = engram_coordinator::snapshot_blob_gc::run_one_snapshot_blob_sweep(
                            state.services.meta.clone(),
                            state.services.blob.clone(),
                            &cfg,
                            engram_coordinator::chunk_gc::SweepMode::Full,
                            &state.services.clock,
                        )
                        .await;
                        let _ = engram_coordinator::bundle_gc::run_one_bundle_sweep(
                            state.services.meta.clone(),
                            state.services.blob.clone(),
                            &cfg,
                            engram_coordinator::chunk_gc::SweepMode::Full,
                            &state.services.clock,
                        )
                        .await;
                    }
                }
            }
            Step::CreateSession => {
                let Some(state) = self.world.replicas.iter().find_map(|r| r.state.clone()) else {
                    return;
                };
                // The create handler's exact persistence shape: reserve
                // (Placed on a fitting host, else Queued) + a create_boot
                // op the SessionOps driver picks up. Uses the world's
                // seeded entropy so ids replay.
                use engram_core::traits::Entropy as _;
                let session_id = SessionId::from(self.world.entropy.uuid());
                let ws = SessionCreateWriteSet {
                    session_id,
                    spec: SessionSpec {
                        image: SIM_IMAGE.into(),
                        // DevVm: no harness child — guest/harness behavior
                        // is an ADR 0098 non-goal, and Agent mode would
                        // require a harness-catalog surface SimMeta
                        // doesn't model yet (D6 candidate).
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
                    ),
                };
                let candidates = self.world.host_ids.clone();
                let disp = state
                    .services
                    .meta
                    .reserve_and_persist_create(ws, &candidates, 0)
                    .await;
                if matches!(disp, Ok(CreateDisposition::Placed(_))) {
                    if let Ok(EnqueueOutcome::Claimed(op)) =
                        engram_coordinator::session_ops::enqueue_claim(
                            &state,
                            session_id,
                            OpKind::CreateBoot,
                            serde_json::json!({}),
                            Some(&format!("create:{session_id}")),
                        )
                        .await
                    {
                        engram_coordinator::session_ops::drive_claimed(&state, op).await;
                    }
                }
                if disp.is_ok() {
                    self.report.sessions_created += 1;
                }
            }
            Step::HostHeartbeats => {
                // Every UP host re-registers + heartbeats, mirroring the
                // real register/heartbeat pair: upsert_host resurrects a
                // detector-killed host (sticky-dead yields ONLY to
                // re-registration) and each live replica re-learns the
                // registry entry the dead_host unregister dropped. Down
                // hosts go silent — exactly what the detector keys on.
                let up: Vec<HostId> = {
                    let hw = self.world.host_world.hosts.lock();
                    hw.iter()
                        .filter(|(_, h)| h.up && !h.heartbeats_partitioned)
                        .map(|(id, _)| *id)
                        .collect()
                };
                let Some(state) = self.world.replicas.iter().find_map(|r| r.state.clone()) else {
                    return;
                };
                for id in up {
                    let hb = sim_heartbeat();
                    let _ = state
                        .services
                        .meta
                        .upsert_host(sim_host_record(id, self.world.clock.clone()))
                        .await;
                    let _ = state.services.meta.touch_host_heartbeat(id, hb).await;
                    // Re-register on every live replica (the register
                    // endpoint's in-memory half).
                    for r in self.world.replicas.iter() {
                        if let Some(rs) = &r.state {
                            rs.host_registry.register(
                                id,
                                std::sync::Arc::new(crate::world::SimHostClient {
                                    host_id: id,
                                    world: self.world.host_world.clone(),
                                    entropy: self.world.entropy.clone(),
                                }),
                            );
                        }
                    }
                }
            }
            Step::CrashHost(i) => {
                let id = self.world.host_ids[i];
                let mut hw = self.world.host_world.hosts.lock();
                if let Some(h) = hw.get_mut(&id) {
                    h.up = false;
                }
            }
            Step::RestartHost(i) => {
                let id = self.world.host_ids[i];
                let mut hw = self.world.host_world.hosts.lock();
                if let Some(h) = hw.get_mut(&id) {
                    h.up = true;
                    // A restarted host machine lost its VMs.
                    h.sandboxes.clear();
                }
            }
            Step::CrashReplica(i) => {
                self.world.replicas[i].state = None;
                self.probe_memory[i].clear();
            }
            Step::RestartReplica(i) => {
                if self.world.replicas[i].state.is_none() {
                    let clock = self.world.replicas[i].clock.clone();
                    self.world.replicas[i].state = Some(self.world.build_replica_with_clock(clock));
                }
            }
            Step::WorkloadBurst(n) => {
                for _ in 0..n {
                    Box::pin(self.execute(Step::CreateSession)).await;
                }
            }
            Step::HeartbeatPartition(i, on) => {
                let id = self.world.host_ids[i];
                let mut hw = self.world.host_world.hosts.lock();
                if let Some(h) = hw.get_mut(&id) {
                    h.heartbeats_partitioned = on;
                }
            }
            Step::RpcHang(i, on) => {
                let id = self.world.host_ids[i];
                let mut hw = self.world.host_world.hosts.lock();
                if let Some(h) = hw.get_mut(&id) {
                    // Far above every op_deadline (Resume/CreateBoot 600s):
                    // the wedge is broken by the deadline, never by the
                    // hang elapsing first.
                    h.rpc_hang = on.then_some(Duration::from_secs(3600));
                }
            }
            Step::ClockSkew(i, secs) => {
                self.world.replicas[i]
                    .clock
                    .set_skew(chrono::Duration::seconds(secs));
            }
            Step::HostCheckpoint(i) => {
                use engram_core::traits::Entropy as _;
                let host_id = self.world.host_ids[i];
                let host_up = {
                    let hw = self.world.host_world.hosts.lock();
                    hw.get(&host_id).is_some_and(|h| h.up)
                };
                if !host_up {
                    return; // a down host's uploader isn't running
                }
                let Some(state) = self.world.replicas.iter().find_map(|r| r.state.clone()) else {
                    return;
                };
                // Every bound Active session on this host gets a
                // recoverable snapshot row — the periodic-checkpoint
                // completion write the host-agent's uploader performs.
                let bound = state
                    .services
                    .meta
                    .list_active_sandbox_assignments_on_host(host_id)
                    .await
                    .unwrap_or_default();
                for (sid, _) in bound {
                    let now = state.services.clock.now_utc();
                    let snap: engram_core::types::snapshot::SnapshotRecord =
                        serde_json::from_value(serde_json::json!({
                            "id": engram_core::SnapshotId::from(self.world.entropy.uuid()),
                            "session_id": sid,
                            "host_id": host_id,
                            "image_version": SIM_IMAGE,
                            "size_bytes": 0,
                            "created_at": now,
                            "last_accessed_at": now,
                            "recoverable": true,
                        }))
                        .expect("sim checkpoint row");
                    let _ = state.services.meta.record_snapshot(snap).await;
                }
            }
            Step::ResumeSession => {
                let Some(state) = self.world.replicas.iter().find_map(|r| r.state.clone()) else {
                    return;
                };
                // Pick the FIRST Idle session (BTreeMap order —
                // deterministic) and drive the real Resume op.
                let idle = self.world.meta.with_db(|db| {
                    db.sessions
                        .values()
                        .find(|r| {
                            r.session.status == engram_core::types::session::SessionState::Idle
                        })
                        .map(|r| r.session.id)
                });
                let Some(sid) = idle else { return };
                if let Ok(EnqueueOutcome::Claimed(op)) =
                    engram_coordinator::session_ops::enqueue_claim(
                        &state,
                        sid,
                        OpKind::Resume,
                        serde_json::json!({}),
                        Some(&format!("resume:{sid}")),
                    )
                    .await
                {
                    engram_coordinator::session_ops::drive_claimed(&state, op).await;
                }
            }
            Step::PgOutage(on) => {
                self.pg_out = on;
                self.world.meta.set_outage(on);
            }
        }
    }

    /// Run `steps` scheduler picks, then quiesce (faults off, generous
    /// time + full driver rounds) and check liveness. Takes `&mut self`
    /// so a failed run's world stays inspectable (trace + state dumps).
    pub async fn run(&mut self, steps: u64) -> Result<SimReport, String> {
        for _ in 0..steps {
            let step = self.pick();
            self.execute(step).await;
            self.report.steps_run += 1;
            if let Err(v) = invariants::check_step(&self.world) {
                return Err(format!(
                    "step {}: {} — {}",
                    self.report.steps_run, v.invariant, v.detail
                ));
            }
        }
        // Quiesce: heal every fault, then round-robin all drivers with
        // time advances until convergence.
        self.world.meta.set_outage(false);
        self.pg_out = false;
        for i in 0..self.world.host_ids.len() {
            self.execute(Step::RestartHost(i)).await;
            self.execute(Step::HeartbeatPartition(i, false)).await;
            self.execute(Step::RpcHang(i, false)).await;
        }
        for i in 0..self.world.replicas.len() {
            self.execute(Step::ClockSkew(i, 0)).await;
        }
        for i in 0..self.world.replicas.len() {
            self.execute(Step::RestartReplica(i)).await;
        }
        // Long enough for exhausted-backoff ops (create_boot's budget
        // is 30 attempts with growing backoff) to either land on the
        // healed fleet or fail terminally — both stable.
        for _ in 0..40 {
            self.execute(Step::HostHeartbeats).await;
            for r in 0..self.world.replicas.len() {
                for kind in DRIVERS {
                    self.execute(Step::Driver(r, kind)).await;
                }
            }
            self.execute(Step::AdvanceTime(Duration::from_secs(120)))
                .await;
        }
        if let Err(v) = invariants::check_quiescence(&self.world) {
            return Err(format!("quiescence: {} — {}", v.invariant, v.detail));
        }
        let seed = self.report.seed;
        Ok(std::mem::replace(
            &mut self.report,
            SimReport {
                seed,
                steps_run: 0,
                sessions_created: 0,
                trace: Vec::new(),
            },
        ))
    }

    /// Post-mortem access for tests and the (D7) failure artifact.
    pub fn report(&self) -> &SimReport {
        &self.report
    }

    pub fn report_mut(&mut self) -> &mut SimReport {
        &mut self.report
    }
}

fn sim_heartbeat() -> engram_core::types::host::HostHeartbeat {
    engram_core::types::host::HostHeartbeat {
        status: engram_core::types::host::HostStatus::Ready,
        capacity: engram_core::types::host::HostCapacity {
            total_gb: 100,
            used_gb: 0,
            total_mib: 32_768,
            used_mib: 0,
            running_sandboxes: 0,
        },
        utilization: sim_utilization(),
        ready_images: Vec::new(),
        current_bundles: Vec::new(),
        total_vcpus: 16,
        wire_version: 1,
        stages_images: false,
        capabilities: Default::default(),
    }
}

fn sim_utilization() -> engram_core::types::host::HostUtilization {
    // allocatable_mib > 0 so pick_host_2d treats the host as measured.
    serde_json::from_value(serde_json::json!({ "allocatable_mib": 24_576 }))
        .expect("utilization from defaults")
}

fn sim_host_record(
    id: HostId,
    clock: Arc<engram_sim::SimClock>,
) -> engram_core::types::host::HostRecord {
    use engram_core::traits::Clock;
    engram_core::types::host::HostRecord {
        id,
        hostname: format!("sim-{id}"),
        cloud_metadata: Default::default(),
        capacity: engram_core::types::host::HostCapacity {
            total_gb: 100,
            used_gb: 0,
            total_mib: 32_768,
            used_mib: 0,
            running_sandboxes: 0,
        },
        utilization: sim_utilization(),
        status: engram_core::types::host::HostStatus::Ready,
        last_heartbeat_at: clock.now_utc(),
        host_addr: None,
        ready_images: Vec::new(),
        current_bundles: Vec::new(),
        cordoned: false,
        total_vcpus: 16,
        wire_version: 1,
        stages_images: false,
        capabilities: Default::default(),
    }
}
