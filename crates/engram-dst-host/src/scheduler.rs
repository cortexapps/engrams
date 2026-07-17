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
    /// Restart: rebuild backends from the surviving store + adopt spools.
    Restart,
    /// Advance virtual time (fires due tokio timers).
    AdvanceTime(Duration),
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
            Step::Restart => "Restart",
            Step::AdvanceTime(..) => "AdvanceTime",
        }
    }
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
                0..=44 => Step::GuestWrite(
                    self.rng.random_range(0..n),
                    self.rng.random_range(0..NUM_CHUNKS),
                ),
                45..=64 => Step::GuestRead(
                    self.rng.random_range(0..n),
                    self.rng.random_range(0..NUM_CHUNKS),
                ),
                65..=79 => Step::FlushTick(self.rng.random_range(0..n)),
                80..=89 => Step::SpoolExport(self.rng.random_range(0..n)),
                90..=96 => Step::SpoolAdopt(self.rng.random_range(0..n)),
                _ => Step::AdvanceTime(Duration::from_secs(self.rng.random_range(1..30))),
            },
            Profile::Chaos => match roll {
                0..=33 => Step::GuestWrite(
                    self.rng.random_range(0..n),
                    self.rng.random_range(0..NUM_CHUNKS),
                ),
                34..=48 => Step::GuestRead(
                    self.rng.random_range(0..n),
                    self.rng.random_range(0..NUM_CHUNKS),
                ),
                49..=61 => Step::FlushTick(self.rng.random_range(0..n)),
                62..=71 => Step::SpoolExport(self.rng.random_range(0..n)),
                72..=80 => Step::SpoolAdopt(self.rng.random_range(0..n)),
                81..=90 => Step::CrashProcess,
                91..=96 => Step::Restart,
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
                self.crashed = true;
            }
            Step::Restart => {
                self.host.restart().await?;
                self.crashed = false;
            }
            Step::AdvanceTime(d) => self.host.clock.advance(d).await,
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
