//! The seeded step scheduler (ADR 0098 Phase 2, P2) — the host-internal
//! mirror of `engram-dst`'s coordinator scheduler.
//!
//! One current-thread tokio runtime with PAUSED time; each step's future
//! runs to completion before the next pick, so a seed replays exactly
//! (per-commit — the lockfile pins tokio; cross-version replay is not
//! promised). Picks come off a forked `ChaCha8Rng` stream; the world's ids
//! come off `SimEntropy`. Iteration that feeds a decision is BTreeMap-ordered
//! or index-based — never a tempdir readdir.

use std::time::Duration;

use rand::Rng;
use rand_chacha::rand_core::SeedableRng;
use rand_chacha::ChaCha8Rng;

use crate::invariants;
use crate::world::{SimHost, NUM_CHUNKS};

/// How many sandboxes each sim host runs. Small (AGENTS.md: size to the
/// property) but ≥2 so cross-sandbox interleavings exist.
pub const NUM_SANDBOXES: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// No crashes. This profile is the guest and flush baseline.
    Calm,
    /// Add Linux process death and recovery to the workload.
    Chaos,
}

/// The simulator step surface. `tests/flow_coverage.rs` matches it without a
/// wildcard. A new variant is therefore a compile error in that test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// A guest write of a fresh content-tagged chunk (appends the ledger).
    GuestWrite(usize, u64),
    /// A guest read-back — asserts read-after-write against the ledger.
    GuestRead(usize, u64),
    /// A full flush: upload dirty, tick + publish the manifest.
    FlushTick(usize),
    /// Process death. The stable dirty files survive.
    CrashProcess,
    /// Restart and recover each stable dirty file.
    Restart,
    /// Advance virtual time (fires due tokio timers).
    AdvanceTime(Duration),
    /// Flow C (P3): one REAL `reconcile_once` tick against the reconcile
    /// world, over the sim-owned strike ledger.
    ReconcileTick,
    /// Flow C perturbation: drop reconcile slot `idx`'s LOCAL binding (the ADR
    /// 0090 survivor). The next `ReconcileTick` must repair it, never reap.
    DropLocalBinding(usize),
    /// Flow C perturbation: revoke reconcile slot `idx`'s coordinator
    /// ownership (a terminal/idle/rebound session). Reconcile should reap it
    /// after the strike debounce.
    RevokeOwnership(usize),
    /// Flow D (P5): begin an eviction finalize for sandbox `idx` — the sim
    /// analog of `snapshot_begin` (drain → stage → durable record → pause).
    /// Idempotent: a pending finalize re-observes the same snapshot id.
    SnapshotBegin(usize),
    /// Flow D (P5): one REAL finalize redrive attempt
    /// (`run_eviction_finalize_attempt`) for sandbox `idx`'s in-flight job.
    FinalizeTick(usize),
    /// Flow D (P5): one finalize attempt for sandbox `idx` under a `CrashFs`
    /// cut at fs-op index `op`, then the process dies. Ops before the cut ran
    /// for real — the on-disk state is exactly a death at that boundary.
    FinalizeCrashAt(usize, usize),
    /// Flow F (P6): the #204 interleaving — a REAL flush parked at the
    /// dirty→pending handoff races a guest read+write of the drained
    /// chunk. The read must never observe pre-drain stale base.
    FlushHandoffRace(usize),
    /// Flow F (P6): the #199 fence interleaving — the migration fence
    /// rises while a REAL flush is parked post-upload/pre-publish; the
    /// publish must abort (manifest unmoved, dirty re-queued) and the
    /// post-heal flush publishes the re-queued writes.
    FlushFenceAbort(usize),
    /// Flow F (P6): process death between `put_manifest` and rebase. The
    /// dirty file keeps all acked writes. Recovery also handles the store
    /// version conflict.
    FlushPreRebaseCrash(usize),
    /// Flow E (P8): open a migration export on sandbox `idx` — the guest
    /// freezes, a REAL export lands in the REAL registry (paused-clock
    /// TTL).
    MigrationBegin(usize),
    /// Flow E (P8): `state.bin` ships — the split-brain flag rises;
    /// `ttl_verdict`'s served arm forbids every later abort-unpause.
    MigrationServeState(usize),
    /// Flow E (P8): a page/artifact serve refreshes the activity TTL.
    MigrationTouch(usize),
    /// Flow E (P8): the dumb-host TTL sweep — REAL `expired()` +
    /// `ttl_verdict` over the coordinator's answer.
    MigrationTtlSweep,
    /// Flow E (P8): coordinator-driven commit (dest owns; source torn
    /// down).
    MigrationCommit(usize),
    /// Flow E (P8): coordinator-driven abort (move never landed; source
    /// un-freezes, zero loss).
    MigrationAbort(usize),
    /// Flow B (P7): rung-2 PARK sandbox `idx` (FC paused, VM resident,
    /// `evicting`-shaped) — the 731df805 pre-condition.
    Park(usize),
    /// Flow B (P7): un-pause (rung-cancel resume) sandbox `idx`. The un-pause
    /// data-plane gate fails fast into `evict_local → resume` if the rootfs
    /// device is not served by this generation — never a dead-plane serve.
    Unpause(usize),
    /// Flow B (P7): the register-time rehydrate sequence — coord-list pass →
    /// local ChainHeadRecord pass (#739) → stale-binding sweep. Swarm uses the
    /// SAFE defaults (list includes parked, local pass on); the adversarial
    /// variants ride the regression seeds.
    RegisterRehydrate,
    /// Flow B (P7): run the stale-binding sweep independently, now GATED by the
    /// Wave 7b classification barrier — reap ONLY the `TerminalSafeToReap` class.
    StaleSweepTick,
    /// Wave 7b (#784 layer 2): LOSE sandbox `idx`'s tracked records (the coord
    /// list and the durable `ChainHeadRecord`) — the #769 gap-A precondition. A
    /// later roll then register leaves a resident guest holding a device NO
    /// record accounts for; the classification barrier must QUARANTINE (not skip,
    /// not sever) it.
    LoseRecord(usize),
    /// Flow B (P7): `try_claim` a spare NBD device on the real allocator (Free
    /// → Claimed) — the slot-accounting + no-double-claim exercise.
    SlotClaim(usize),
    /// Flow B (P7): release the oldest held spare lease (Claimed → Free),
    /// exercising the allocator's release path.
    SlotPopulateTick,
    /// #898: resume a terminally-finalized (destroyed) sandbox from its
    /// finalize-published snapshot — the production-equivalent recovery
    /// path the coordinator drives after eviction, decided by the REAL
    /// `plan_resume_attach`. Restart no longer resurrects destroyed slots
    /// as survivors, so THIS step is where a finalize that published the
    /// wrong manifest (the #897 class) meets the acked-write oracle. No-op
    /// unless the slot is terminally destroyed and non-pending; the swarm
    /// resumes GATED (safe default), the ungated leg rides the G2 seeds.
    FinalizedResume(usize),
}

impl Step {
    /// A stable coverage name for the flow-coverage meta-test. Wildcard-free:
    /// a new [`Step`] variant that is not named here is a compile error.
    pub fn coverage_name(&self) -> &'static str {
        match self {
            Step::GuestWrite(..) => "GuestWrite",
            Step::GuestRead(..) => "GuestRead",
            Step::FlushTick(..) => "FlushTick",
            Step::CrashProcess => "CrashProcess",
            Step::Restart => "Restart",
            Step::AdvanceTime(..) => "AdvanceTime",
            Step::ReconcileTick => "ReconcileTick",
            Step::DropLocalBinding(..) => "DropLocalBinding",
            Step::RevokeOwnership(..) => "RevokeOwnership",
            Step::SnapshotBegin(..) => "SnapshotBegin",
            Step::FinalizeTick(..) => "FinalizeTick",
            Step::FinalizeCrashAt(..) => "FinalizeCrashAt",
            Step::FlushHandoffRace(..) => "FlushHandoffRace",
            Step::FlushFenceAbort(..) => "FlushFenceAbort",
            Step::FlushPreRebaseCrash(..) => "FlushPreRebaseCrash",
            Step::MigrationBegin(..) => "MigrationBegin",
            Step::MigrationServeState(..) => "MigrationServeState",
            Step::MigrationTouch(..) => "MigrationTouch",
            Step::MigrationTtlSweep => "MigrationTtlSweep",
            Step::MigrationCommit(..) => "MigrationCommit",
            Step::MigrationAbort(..) => "MigrationAbort",
            Step::Park(..) => "Park",
            Step::Unpause(..) => "Unpause",
            Step::RegisterRehydrate => "RegisterRehydrate",
            Step::StaleSweepTick => "StaleSweepTick",
            Step::LoseRecord(..) => "LoseRecord",
            Step::SlotClaim(..) => "SlotClaim",
            Step::SlotPopulateTick => "SlotPopulateTick",
            Step::FinalizedResume(..) => "FinalizedResume",
        }
    }
}

/// Select an operation index for a `CrashFs` finalize cut.
#[cfg(target_os = "linux")]
fn pick_op_index(rng: &mut ChaCha8Rng) -> usize {
    rng.random_range(0..32usize)
}

/// The run artifact — same shape as `engram-dst`'s `SimReport`. The
/// replay-diff test compares two runs of one seed on `trace`.
#[derive(Debug)]
pub struct SimReport {
    pub seed: u64,
    pub steps_run: u64,
    pub writes_acked: u64,
    pub trace: Vec<String>,
}

pub struct Sim {
    pub host: SimHost,
    rng: ChaCha8Rng,
    profile: Profile,
    /// True while the process is "crashed" (RAM backends dropped, awaiting a
    /// Restart) — used only to weight the pick toward a restart.
    crashed: bool,
    /// Flow C (P3): the teardown-reconcile strike ledger, owned by the sim
    /// across ticks (the `reconcile_once` `strikes` parameter). A
    /// `CrashProcess` clears it — a fresh host-agent process starts with an
    /// empty ledger, exactly like the real interval wrapper's local
    /// `HashMap`. `HashMap` (not `BTreeMap`) mirrors the prod signature; it is
    /// keyed-access only inside `reconcile_once` (never iterated for a
    /// decision), so it is not a determinism leak.
    reconcile_strikes: std::collections::HashMap<engram_core::SandboxId, u32>,
    report: SimReport,
}

impl Sim {
    /// Build a sim for `seed`. Async because seeding the store + backends
    /// awaits deterministic content-addressed I/O.
    pub async fn new(seed: u64, profile: Profile) -> Self {
        // Forked stream: the scheduler's picks use an offset of the seed so
        // adding a world-entropy consumer doesn't shift the pick sequence
        // (mirrors engram-dst).
        let rng = ChaCha8Rng::seed_from_u64(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let host = SimHost::new(seed, NUM_SANDBOXES).await;
        Self {
            host,
            rng,
            profile,
            crashed: false,
            reconcile_strikes: std::collections::HashMap::new(),
            report: SimReport {
                seed,
                steps_run: 0,
                writes_acked: 0,
                trace: Vec::new(),
            },
        }
    }

    fn pick(&mut self) -> Step {
        let n = NUM_SANDBOXES;
        // A crashed Linux process must restart before other work can run.
        #[cfg(target_os = "linux")]
        if self.crashed {
            let roll: u32 = self.rng.random_range(0..100);
            if roll < 80 {
                return Step::Restart;
            }
            return Step::AdvanceTime(Duration::from_secs(self.rng.random_range(1..30)));
        }
        // 0..114: 0..=99 the pre-P8 menu, 100..=106 the migration
        // lifecycle, then the profile-specific fault menu, the tail
        // AdvanceTime. Widening the range re-shuffles old seeds'
        // exploration (fine — seeds pin to a commit).
        let roll: u32 = self.rng.random_range(0..114);
        // Weights are part of the seed contract: changing them makes old
        // seeds explore differently (fine — seeds pin to a commit), but the
        // pick must NEVER branch on anything non-deterministic.
        match self.profile {
            Profile::Calm => match roll {
                0..=32 => Step::GuestWrite(
                    self.rng.random_range(0..n),
                    self.rng.random_range(0..NUM_CHUNKS),
                ),
                33..=47 => Step::GuestRead(
                    self.rng.random_range(0..n),
                    self.rng.random_range(0..NUM_CHUNKS),
                ),
                48..=57 => Step::FlushTick(self.rng.random_range(0..n)),
                58..=63 => Step::FlushTick(self.rng.random_range(0..n)),
                64..=69 => Step::GuestWrite(
                    self.rng.random_range(0..n),
                    self.rng.random_range(0..NUM_CHUNKS),
                ),
                70..=75 => Step::ReconcileTick,
                76..=78 => Step::DropLocalBinding(self.rng.random_range(0..n)),
                79..=80 => Step::RevokeOwnership(self.rng.random_range(0..n)),
                81..=83 => Step::AdvanceTime(Duration::from_secs(self.rng.random_range(1..30))),
                // Flow B (P7): the slot/reattach lifecycle. All benign under
                // the safe defaults — the oracles must hold every step.
                84..=86 => Step::SlotClaim(self.rng.random_range(0..n)),
                87..=88 => Step::SlotPopulateTick,
                89..=90 => Step::Park(self.rng.random_range(0..n)),
                91..=92 => Step::Unpause(self.rng.random_range(0..n)),
                93..=94 => Step::RegisterRehydrate,
                95 => Step::StaleSweepTick,
                // Flow D (P5): the graceful finalize belongs in the calm
                // durability baseline (begin → tick → tick … completes).
                96 => Step::SnapshotBegin(self.rng.random_range(0..n)),
                97 => Step::FinalizeTick(self.rng.random_range(0..n)),
                // Flow F (P6): both fault-free-converging interleavings run
                // in the calm baseline too.
                98 => Step::FlushHandoffRace(self.rng.random_range(0..n)),
                99 => Step::FlushFenceAbort(self.rng.random_range(0..n)),
                // Flow E (P8): the migration lifecycle (benign under the
                // honest coordinator; abort/commit converge).
                100..=101 => Step::MigrationBegin(self.rng.random_range(0..n)),
                102 => Step::MigrationServeState(self.rng.random_range(0..n)),
                103 => Step::MigrationTouch(self.rng.random_range(0..n)),
                104 => Step::MigrationTtlSweep,
                105 => Step::MigrationCommit(self.rng.random_range(0..n)),
                106 => Step::MigrationAbort(self.rng.random_range(0..n)),
                107 => Step::FlushHandoffRace(self.rng.random_range(0..n)),
                108 => Step::FlushFenceAbort(self.rng.random_range(0..n)),
                // #898: the graceful post-eviction resume belongs in the calm
                // baseline (begin → ticks → destroy → resume → reads verify).
                // Carved from the tail AdvanceTime band, not a re-weight.
                109 => Step::FinalizedResume(self.rng.random_range(0..n)),
                _ => Step::AdvanceTime(Duration::from_secs(self.rng.random_range(1..30))),
            },
            Profile::Chaos => match roll {
                0..=22 => Step::GuestWrite(
                    self.rng.random_range(0..n),
                    self.rng.random_range(0..NUM_CHUNKS),
                ),
                23..=32 => Step::GuestRead(
                    self.rng.random_range(0..n),
                    self.rng.random_range(0..NUM_CHUNKS),
                ),
                33..=41 => Step::FlushTick(self.rng.random_range(0..n)),
                42..=48 => Step::GuestWrite(
                    self.rng.random_range(0..n),
                    self.rng.random_range(0..NUM_CHUNKS),
                ),
                49..=55 => Step::FlushTick(self.rng.random_range(0..n)),
                56..=62 => Step::ReconcileTick,
                63..=65 => Step::DropLocalBinding(self.rng.random_range(0..n)),
                66..=67 => Step::RevokeOwnership(self.rng.random_range(0..n)),
                68..=76 => {
                    #[cfg(target_os = "linux")]
                    {
                        Step::CrashProcess
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        Step::GuestWrite(
                            self.rng.random_range(0..n),
                            self.rng.random_range(0..NUM_CHUNKS),
                        )
                    }
                }
                77..=79 => {
                    #[cfg(target_os = "linux")]
                    {
                        Step::Restart
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        Step::AdvanceTime(Duration::from_secs(self.rng.random_range(1..30)))
                    }
                }
                80..=83 => Step::ReconcileTick,
                84 => {
                    #[cfg(target_os = "linux")]
                    {
                        Step::FinalizeCrashAt(
                            self.rng.random_range(0..n),
                            pick_op_index(&mut self.rng),
                        )
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        Step::FinalizeTick(self.rng.random_range(0..n))
                    }
                }
                // Flow D (P5): the finalize lifecycle under chaos.
                85 => Step::SnapshotBegin(self.rng.random_range(0..n)),
                86..=87 => Step::FinalizeTick(self.rng.random_range(0..n)),
                // Flow B (P7): park, rehydrate, sweep, and unpause.
                88..=89 => Step::SlotClaim(self.rng.random_range(0..n)),
                90 => Step::SlotPopulateTick,
                91..=92 => Step::Park(self.rng.random_range(0..n)),
                93..=94 => Step::Unpause(self.rng.random_range(0..n)),
                95 => Step::RegisterRehydrate,
                96 => Step::StaleSweepTick,
                // Flow F (P6): the flush-pipeline interleavings.
                97 => Step::FlushHandoffRace(self.rng.random_range(0..n)),
                98 => Step::FlushFenceAbort(self.rng.random_range(0..n)),
                99 => {
                    #[cfg(target_os = "linux")]
                    {
                        Step::FlushPreRebaseCrash(self.rng.random_range(0..n))
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        Step::FlushFenceAbort(self.rng.random_range(0..n))
                    }
                }
                // Flow E (P8): the migration lifecycle under chaos (crash
                // steps above kill the RAM registry mid-export).
                100..=101 => Step::MigrationBegin(self.rng.random_range(0..n)),
                102 => Step::MigrationServeState(self.rng.random_range(0..n)),
                103 => Step::MigrationTouch(self.rng.random_range(0..n)),
                104 => Step::MigrationTtlSweep,
                105 => Step::MigrationCommit(self.rng.random_range(0..n)),
                106 => Step::MigrationAbort(self.rng.random_range(0..n)),
                // Wave 7b (#784 layer 2): a small-weight record-loss fault so the
                // gap-A family — a resident survivor invisible to the records —
                // arises under the swarm and the barrier's severed-live-holder +
                // quarantine oracles guard it every step.
                107..=109 => Step::LoseRecord(self.rng.random_range(0..n)),
                110 => Step::FlushHandoffRace(self.rng.random_range(0..n)),
                111 => Step::FlushFenceAbort(self.rng.random_range(0..n)),
                // #898: the post-eviction resume under chaos — the crash
                // steps above interleave rolls between destroy and resume.
                // Carved from the tail AdvanceTime band, not a re-weight.
                112 => Step::FinalizedResume(self.rng.random_range(0..n)),
                _ => Step::AdvanceTime(Duration::from_secs(self.rng.random_range(1..30))),
            },
        }
    }

    async fn execute(&mut self, step: Step) -> Result<(), String> {
        self.report.trace.push(format!("{step:?}"));
        match step {
            Step::GuestWrite(idx, c) => {
                let before = self.host.ledger.len();
                self.host.guest_write(idx, c).await?;
                if self.host.ledger.len() > before {
                    self.report.writes_acked += 1;
                }
            }
            Step::GuestRead(idx, c) => self.host.guest_read(idx, c).await?,
            Step::FlushTick(idx) => self.host.flush_tick(idx).await?,
            Step::CrashProcess => {
                self.host.crash_process().await?;
                // A fresh host-agent process starts with an empty strike
                // ledger (the real wrapper's local `HashMap`).
                self.reconcile_strikes.clear();
                self.crashed = true;
            }
            Step::Restart => {
                #[cfg(target_os = "linux")]
                {
                    self.host.restart().await?;
                    self.crashed = false;
                }
                #[cfg(not(target_os = "linux"))]
                return Err("dirty-file recovery needs Linux extent semantics".to_string());
            }
            Step::AdvanceTime(d) => self.host.clock.advance(d).await,
            Step::ReconcileTick => {
                self.host
                    .reconcile_tick(&mut self.reconcile_strikes)
                    .await?;
            }
            Step::DropLocalBinding(idx) => self.host.drop_local_binding(idx),
            Step::RevokeOwnership(idx) => self.host.revoke_ownership(idx),
            Step::SnapshotBegin(idx) => {
                let _ = self.host.snapshot_begin(idx).await?;
            }
            Step::FinalizeTick(idx) => self.host.finalize_tick(idx).await?,
            Step::FinalizeCrashAt(idx, op) => {
                self.host.finalize_crash_at(idx, op).await?;
                self.reconcile_strikes.clear();
                self.crashed = true;
            }
            Step::FlushHandoffRace(idx) => self.host.flush_handoff_race(idx).await?,
            Step::FlushFenceAbort(idx) => self.host.flush_fence_abort(idx).await?,
            Step::FlushPreRebaseCrash(idx) => {
                self.host.flush_pre_rebase_crash(idx).await?;
                self.reconcile_strikes.clear();
                self.crashed = true;
            }
            Step::MigrationBegin(idx) => {
                let _ = self.host.migration_begin(idx).await?;
            }
            Step::MigrationServeState(idx) => self.host.migration_serve_state(idx),
            Step::MigrationTouch(idx) => self.host.migration_touch(idx),
            Step::MigrationTtlSweep => self.host.migration_ttl_sweep().await?,
            Step::MigrationCommit(idx) => self.host.migration_commit(idx),
            Step::MigrationAbort(idx) => self.host.migration_abort(idx),
            Step::Park(idx) => self.host.park(idx),
            // The un-pause gate firing (unserved plane) is CORRECT behavior, not
            // a step failure — the guest stays parked, routed to recovery.
            Step::Unpause(idx) => {
                let _ = self.host.unpause(idx);
            }
            // Swarm uses the SAFE defaults (coord list includes parked survivors,
            // #739 local pass on) so the oracles hold every step; the adversarial
            // #739 variants ride the regression seeds.
            Step::RegisterRehydrate => self.host.register_rehydrate(true, true).await?,
            Step::StaleSweepTick => self.host.stale_sweep_tick(),
            Step::LoseRecord(idx) => self.host.lose_record(idx),
            Step::SlotClaim(idx) => self.host.slot_claim(idx).await,
            Step::SlotPopulateTick => self.host.slot_populate_tick(),
            Step::FinalizedResume(idx) => {
                // A dead host-agent serves no resume: the coordinator can only
                // drive this against a live process (the crashed window's other
                // steps no-op naturally on a None backend; this one would not).
                if !self.crashed {
                    self.host.finalized_resume(idx).await?;
                }
            }
        }
        Ok(())
    }

    /// Run selected steps and check the oracles after each step. Linux also
    /// runs a final process death and recovery. The final flush must publish
    /// every surviving acked write.
    pub async fn run(&mut self, steps: u64) -> Result<SimReport, String> {
        for _ in 0..steps {
            let step = self.pick();
            self.execute(step).await?;
            self.report.steps_run += 1;
            if let Err(v) = invariants::check(&self.host).await {
                return Err(format!(
                    "step {}: {} — {}",
                    self.report.steps_run, v.invariant, v.detail
                ));
            }
        }
        #[cfg(target_os = "linux")]
        {
            self.execute(Step::CrashProcess).await?;
            self.execute(Step::Restart).await?;
        }
        if let Err(v) = invariants::check(&self.host).await {
            return Err(format!("quiescence: {} — {}", v.invariant, v.detail));
        }
        // Flow D drain (oracle #8): every started finalize must converge to
        // completed-or-quarantined within the attempts budget once faults are
        // healed (post-restart the fs is the honest TokioFs again). Each
        // round gives every slot one real redrive attempt + the backoff's
        // virtual time.
        for _ in 0..(crate::world::SIM_FINALIZE_MAX_ATTEMPTS as u64 + 2) {
            if self.host.pending_finalizes.is_empty() {
                break;
            }
            for idx in 0..NUM_SANDBOXES {
                self.execute(Step::FinalizeTick(idx)).await?;
            }
            self.execute(Step::AdvanceTime(Duration::from_secs(600)))
                .await?;
        }
        if let Err(v) = invariants::check_finalize_convergence(&self.host) {
            return Err(format!("quiescence: {} — {}", v.invariant, v.detail));
        }
        if let Err(v) = invariants::check(&self.host).await {
            return Err(format!(
                "quiescence-post-drain: {} — {}",
                v.invariant, v.detail
            ));
        }
        // End each open migration before the final flush. Linux process death
        // already clears the registry. This also heals the macOS path, which
        // does not run dirty-file recovery.
        for idx in 0..NUM_SANDBOXES {
            self.host.migration_abort(idx);
        }
        // The quiescence flush must publish the latest acked content.
        for idx in 0..NUM_SANDBOXES {
            self.execute(Step::FlushTick(idx)).await?;
        }
        if let Err(v) = invariants::check(&self.host).await {
            return Err(format!(
                "quiescence-post-flush: {} — {}",
                v.invariant, v.detail
            ));
        }
        if let Err(v) = invariants::check_quiescent_floor(&self.host).await {
            return Err(format!(
                "quiescence-post-flush: {} — {}",
                v.invariant, v.detail
            ));
        }
        let seed = self.report.seed;
        Ok(std::mem::replace(
            &mut self.report,
            SimReport {
                seed,
                steps_run: 0,
                writes_acked: 0,
                trace: Vec::new(),
            },
        ))
    }

    /// Post-mortem access for the CLI failure artifact and tests.
    pub fn report(&self) -> &SimReport {
        &self.report
    }
}
