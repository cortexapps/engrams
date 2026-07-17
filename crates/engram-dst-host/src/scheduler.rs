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
    /// No crashes — pure guest workload + flush/spool machinery. The
    /// durability baseline.
    Calm,
    /// Adds process crash/restart interleavings on top of the workload.
    Chaos,
}

/// The P2 step surface — all drivable TODAY with the portable components
/// (`ChunkedDiskBackend` + the shutdown spool). P3+ extends this enum (Flow
/// A–E extractions, crash-point injection); `tests/flow_coverage.rs` matches
/// it wildcard-free so a new variant is a compile error, not a silent gap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// A guest write of a fresh content-tagged chunk (appends the ledger).
    GuestWrite(usize, u64),
    /// A guest read-back — asserts read-after-write against the ledger.
    GuestRead(usize, u64),
    /// A full flush: upload dirty, tick + publish the manifest.
    FlushTick(usize),
    /// Export the un-uploaded tier to the shutdown spool.
    SpoolExport(usize),
    /// Successor spool adoption (race-free self-handoff).
    SpoolAdopt(usize),
    /// Orderly process crash: spool live sandboxes, then drop RAM backends.
    CrashProcess,
    /// ABRUPT process death (P4.5): drop RAM backends with NO shutdown spool —
    /// the post-ack / pre-handoff loss window. An un-handed-off acked write is
    /// legitimately lost; the honest oracle tolerates exactly that.
    AbruptCrash,
    /// Restart: rebuild backends from the surviving store + adopt spools.
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
    /// Flow A (P4): drive the REAL extracted SIGTERM ladder. The payload is
    /// the seeded `ENGRAM_SHUTDOWN_FLUSH_BUDGET_SECS` in MILLIseconds (`None`
    /// = env unset); `u64` millis keeps [`Step`] `Eq` (an `f64` would not).
    /// A tiny budget overruns the final-flush deadline → the #225 shape.
    Sigterm(Option<u64>),
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
    /// P5 (replacing P4's post-hoc spool mangle): the predecessor's spool
    /// write is cut at fs-op index `op` by the real seam (redundant with a
    /// completed flush-publish), then the process dies. Every op index must
    /// recover every acked write.
    SpoolCrashAt(usize),
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
    /// Flow B (P7): run the stale-binding sweep independently (DISCONNECT
    /// devices whose recorded owner is a dead generation).
    StaleSweepTick,
    /// Flow B (P7): `try_claim` a spare NBD device on the real allocator (Free
    /// → Claimed) — the slot-accounting + no-double-claim exercise.
    SlotClaim(usize),
    /// Flow B (P7): release the oldest held spare lease (Claimed → Free),
    /// exercising the allocator's release path.
    SlotPopulateTick,
}

impl Step {
    /// A stable coverage name for the flow-coverage meta-test. Wildcard-free:
    /// a new [`Step`] variant that is not named here is a compile error.
    pub fn coverage_name(&self) -> &'static str {
        match self {
            Step::GuestWrite(..) => "GuestWrite",
            Step::GuestRead(..) => "GuestRead",
            Step::FlushTick(..) => "FlushTick",
            Step::SpoolExport(..) => "SpoolExport",
            Step::SpoolAdopt(..) => "SpoolAdopt",
            Step::CrashProcess => "CrashProcess",
            Step::AbruptCrash => "AbruptCrash",
            Step::Restart => "Restart",
            Step::AdvanceTime(..) => "AdvanceTime",
            Step::ReconcileTick => "ReconcileTick",
            Step::DropLocalBinding(..) => "DropLocalBinding",
            Step::RevokeOwnership(..) => "RevokeOwnership",
            Step::Sigterm(..) => "Sigterm",
            Step::SnapshotBegin(..) => "SnapshotBegin",
            Step::FinalizeTick(..) => "FinalizeTick",
            Step::FinalizeCrashAt(..) => "FinalizeCrashAt",
            Step::SpoolCrashAt(..) => "SpoolCrashAt",
            Step::Park(..) => "Park",
            Step::Unpause(..) => "Unpause",
            Step::RegisterRehydrate => "RegisterRehydrate",
            Step::StaleSweepTick => "StaleSweepTick",
            Step::SlotClaim(..) => "SlotClaim",
            Step::SlotPopulateTick => "SlotPopulateTick",
        }
    }
}

/// A seeded final-flush budget in milliseconds (`None` = env unset). The set
/// spans below and above [`SIM_FLUSH_COST`](crate::world::SIM_FLUSH_COST) (1 s)
/// so both the deadline-overrun (#225) and the clean-flush ladder paths are
/// exercised; `Some(0)` is the non-positive-env case (`plan_shutdown` defaults
/// it, no overrun).
fn pick_budget_ms(rng: &mut ChaCha8Rng) -> Option<u64> {
    match rng.random_range(0..6u32) {
        0 => None,         // env unset → default 20 s → no overrun
        1 => Some(0),      // non-positive → default 20 s → no overrun
        2 => Some(1),      // 1 ms → OVERRUN (#225)
        3 => Some(500),    // 0.5 s → OVERRUN (#225)
        4 => Some(5_000),  // 5 s → completes
        _ => Some(30_000), // 30 s → completes
    }
}

/// A seeded fs-op cut index for the CrashFs injectors. `write_spool` over a
/// full dirty set traces ~22 ops and a finalize pass ~30; 0..32 covers every
/// boundary plus past-the-end (which completes, then dies) — all legitimate.
fn pick_op_index(rng: &mut ChaCha8Rng) -> usize {
    rng.random_range(0..32usize)
}

/// Convert a seeded budget (millis) to the `plan_shutdown` env value (secs).
fn budget_secs(ms: Option<u64>) -> Option<f64> {
    ms.map(|ms| ms as f64 / 1000.0)
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
        // A crashed process must restart before doing anything else useful;
        // bias hard toward Restart so the crash window stays bounded.
        if self.crashed {
            let roll: u32 = self.rng.random_range(0..100);
            if roll < 80 {
                return Step::Restart;
            }
            return Step::AdvanceTime(Duration::from_secs(self.rng.random_range(1..30)));
        }
        let roll: u32 = self.rng.random_range(0..100);
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
                58..=63 => Step::SpoolExport(self.rng.random_range(0..n)),
                64..=69 => Step::SpoolAdopt(self.rng.random_range(0..n)),
                70..=75 => Step::ReconcileTick,
                76..=78 => Step::DropLocalBinding(self.rng.random_range(0..n)),
                79..=80 => Step::RevokeOwnership(self.rng.random_range(0..n)),
                // Sigterm is a GRACEFUL shutdown (the spool always completes),
                // so it belongs in the calm durability baseline too.
                81..=83 => Step::Sigterm(pick_budget_ms(&mut self.rng)),
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
                97..=98 => Step::FinalizeTick(self.rng.random_range(0..n)),
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
                42..=48 => Step::SpoolExport(self.rng.random_range(0..n)),
                49..=55 => Step::SpoolAdopt(self.rng.random_range(0..n)),
                56..=62 => Step::ReconcileTick,
                63..=65 => Step::DropLocalBinding(self.rng.random_range(0..n)),
                66..=67 => Step::RevokeOwnership(self.rng.random_range(0..n)),
                68..=72 => Step::CrashProcess,
                // P4.5: abrupt death exercises the post-ack/pre-handoff loss
                // window — the honest oracle must TOLERATE the resulting rolled-
                // back un-handed-off writes (and still catch a lost handed-off
                // one). Without this the hard-crash window is never explored.
                73..=76 => Step::AbruptCrash,
                77..=79 => Step::Restart,
                80..=82 => Step::Sigterm(pick_budget_ms(&mut self.rng)),
                // P5: the op-boundary crash injectors (CrashFs cuts).
                83 => Step::SpoolCrashAt(pick_op_index(&mut self.rng)),
                84 => {
                    Step::FinalizeCrashAt(self.rng.random_range(0..n), pick_op_index(&mut self.rng))
                }
                // Flow D (P5): the finalize lifecycle under chaos.
                85 => Step::SnapshotBegin(self.rng.random_range(0..n)),
                86..=87 => Step::FinalizeTick(self.rng.random_range(0..n)),
                // Flow B (P7): park → roll → rehydrate → sweep → un-pause. The
                // roll comes from CrashProcess/AbruptCrash/Sigterm/the injectors.
                88..=89 => Step::SlotClaim(self.rng.random_range(0..n)),
                90 => Step::SlotPopulateTick,
                91..=92 => Step::Park(self.rng.random_range(0..n)),
                93..=94 => Step::Unpause(self.rng.random_range(0..n)),
                95..=96 => Step::RegisterRehydrate,
                97..=98 => Step::StaleSweepTick,
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
            Step::SpoolExport(idx) => self.host.spool_export(idx).await?,
            Step::SpoolAdopt(idx) => self.host.spool_adopt(idx).await?,
            Step::CrashProcess => {
                self.host.crash_process().await?;
                // A fresh host-agent process starts with an empty strike
                // ledger (the real wrapper's local `HashMap`).
                self.reconcile_strikes.clear();
                self.crashed = true;
            }
            Step::AbruptCrash => {
                self.host.abrupt_crash().await?;
                // Same successor-process reset as any roll; RAM died, so bias
                // toward Restart next.
                self.reconcile_strikes.clear();
                self.crashed = true;
            }
            Step::Restart => {
                self.host.restart().await?;
                self.crashed = false;
            }
            Step::AdvanceTime(d) => self.host.clock.advance(d).await,
            Step::ReconcileTick => {
                self.host
                    .reconcile_tick(&mut self.reconcile_strikes)
                    .await?;
            }
            Step::DropLocalBinding(idx) => self.host.drop_local_binding(idx),
            Step::RevokeOwnership(idx) => self.host.revoke_ownership(idx),
            Step::Sigterm(budget_ms) => {
                self.host.sigterm(budget_secs(budget_ms)).await?;
                // A fresh host-agent process starts with an empty strike
                // ledger; RAM died, so bias toward Restart next.
                self.reconcile_strikes.clear();
                self.crashed = true;
            }
            Step::SnapshotBegin(idx) => {
                let _ = self.host.snapshot_begin(idx).await?;
            }
            Step::FinalizeTick(idx) => self.host.finalize_tick(idx).await?,
            Step::FinalizeCrashAt(idx, op) => {
                self.host.finalize_crash_at(idx, op).await?;
                self.reconcile_strikes.clear();
                self.crashed = true;
            }
            Step::SpoolCrashAt(op) => {
                self.host.spool_crash_at(op).await?;
                self.reconcile_strikes.clear();
                self.crashed = true;
            }
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
            Step::SlotClaim(idx) => self.host.slot_claim(idx).await,
            Step::SlotPopulateTick => self.host.slot_populate_tick(),
        }
        Ok(())
    }

    /// Run `steps` picks, checking the oracle after every step, then quiesce
    /// (a final orderly crash→restart) and re-check — the durability property
    /// must hold across a clean shutdown/recovery cycle too.
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
        // Quiesce: one clean crash→restart cycle, then the oracle must still
        // recover every acked write.
        self.execute(Step::CrashProcess).await?;
        self.execute(Step::Restart).await?;
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
