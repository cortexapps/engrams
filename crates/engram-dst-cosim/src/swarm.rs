//! The seeded boundary swarm (ADR 0098 R-CoSim, rung 2, #784 deliverable 2).
//!
//! Rung 1 is directed-only. This is the swarm arm: a seeded scheduler picks
//! interleavings of REAL coordinator drivers, REAL host steps, roll/crash
//! faults, AND **bridge faults** over the coordinator↔host boundary, checking
//! the standing oracles after every step and at quiescence.
//!
//! # Small-world bounding (the state-space product is the risk)
//!
//! The world is deliberately tiny — **1 coordinator replica, 1 host, ≤3 live
//! sessions** — because the co-simulated state space is the product of the
//! coordinator's session FSM, the host's device plane, the reconcile world,
//! AND the bridge-fault interleavings. A larger world multiplies horizons
//! without exploring a qualitatively new class at the BOUNDARY (which is what
//! this rung exists for); the fault profile is biased to the boundary family
//! (roll / rehydrate / sweep / un-pause / straggler / bind-loss) so the seeds
//! spend their steps where the risk lives, not on breadth.
//!
//! # The bridge faults (the applied-commit-but-ack-lost window)
//!
//! The mutating boundary is `bind` / `unbind` / `destroy` / the host→coord
//! publish. Rather than a transparently-interposed effect queue (the
//! `engram-dst` R2 pattern, whose transparent form is a fidelity refinement
//! tracked in the ADR), the swarm reaches the SAME reachable states via explicit
//! perturbation steps that mirror the effect outcomes:
//!
//! * [`Step::DropHostBinding`] — the coordinator COMMITTED a bind to PG but the
//!   host-side bind effect was lost (a dropped/never-delivered RPC): the host's
//!   in-RAM binding is gone while PG still owns it. This is the applied-commit-
//!   but-ack-lost window; the reconcile Unbound arm must REPAIR it (never reap).
//! * [`Step::ForceHostLost`] — the dead-host detector flips a survivor while its
//!   VM keeps running (the partition window the straggler sweep must ask-the-host
//!   about, #777).
//! * A lost DESTROY is the leftover-VM-under-a-terminal-session state
//!   ([`Step::ForceTerminal`]): the coordinator disowns but the host kept the VM;
//!   the reconcile must reap it.
//!
//! # Determinism (audit items 6–8)
//!
//! Each seed runs on its OWN current-thread paused-clock runtime dropped at the
//! boundary ([`run_seed`], the `engram-dst` shape) + a named 600 s watchdog in
//! the CLI. Time is driven EXPLICITLY in coarse steps (never relied-on
//! auto-advance for a decision boundary — the clock-threshold coordinator steps
//! are the straggler sweep's 60 s and the idle detector, both crossed by coarse
//! [`Step::AdvanceTime`] jumps well past the threshold). Picks come off a forked
//! `ChaCha8Rng`; world ids come off `SimEntropy`; iteration that feeds a
//! decision is `BTreeMap`/index-ordered. The replay-twice self-check
//! (`tests/swarm_replay.rs` + the CLI) is the determinism guard.

use engram_core::types::BindingDisposition;
use rand::Rng;
use rand_chacha::rand_core::SeedableRng;
use rand_chacha::ChaCha8Rng;

use engram_core::types::session::SessionState;
use engram_core::SessionId;

use crate::scheduler::Cosim;

/// The swarm profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// No process rolls/crashes — the boundary lifecycle + bridge-loss faults
    /// only. The convergence baseline.
    Calm,
    /// Adds host-agent rolls (the survivor-rehydrate family's trigger) on top.
    Chaos,
}

/// One swarm step. Wildcard-free coverage name (a new variant is a compile
/// error in the coverage meta-test).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Create + boot a fresh session to Active.
    BootSession,
    /// Guest work on session slot `.0` (`units` writes).
    GuestWork(usize, u32),
    /// Record a periodic checkpoint for session slot `.0`.
    PeriodicCheckpoint(usize),
    /// Idle-evict session slot `.0` to Idle via the REAL D5 path.
    EvictToIdle(usize),
    /// Resume session slot `.0` from Idle.
    Resume(usize),
    /// Rung-2 park session slot `.0`.
    Park(usize),
    /// Un-pause session slot `.0` (the REAL data-plane gate).
    Unpause(usize),
    /// Host-agent roll (new generation; survivors resident).
    Roll,
    /// Register-rehydrate against the REAL coordinator listing (`.0` = the #739
    /// local pass on/off).
    RegisterRehydrate(bool),
    /// The stale-binding sweep (REAL `sweep_verdict`).
    StaleSweep,
    /// The FC guest for session slot `.0` genuinely dies (#806 holder gone).
    KillGuest(usize),
    /// Wave 7b (#784 layer 2): lose session slot `.0`'s tracked records (#769
    /// gap A) — a resident guest holds a device no record accounts for.
    LoseRecord(usize),
    /// One teardown-reconcile tick (`.0` = honor the capture signal).
    ReconcileTick(bool),
    /// One coordinator `host_lost_straggler_sweep` tick (#782/#777).
    StragglerSweep,
    /// Bridge fault: the host-side bind for session slot `.0` was lost (PG
    /// committed) — the applied-commit-but-ack-lost window.
    DropHostBinding(usize),
    /// Fault: the dead-host detector flips session slot `.0` to HostLost (VM
    /// survives).
    ForceHostLost(usize),
    /// Fault: session slot `.0` fails terminally (a lost destroy leaves the VM).
    ForceTerminal(usize),
    /// Drive one finalize attempt per in-flight capture.
    FinalizePending,
    /// `try_claim` a spare NBD device (slot-accounting).
    SlotClaim,
    /// Release the oldest spare lease (slot-accounting).
    SlotPopulate,
    /// The idle detector nominates over-idle sessions.
    IdleDetector,
    /// Advance virtual time (coarse — crosses decision thresholds cleanly).
    AdvanceTime(u64),
}

/// The maximum live sessions in the small world (the state-space bound).
pub const MAX_SESSIONS: usize = 3;

/// The run artifact (same shape as the sibling sims' `SimReport`).
#[derive(Debug)]
pub struct SwarmReport {
    pub seed: u64,
    pub steps_run: u64,
    pub sessions_created: u64,
    pub trace: Vec<String>,
}

/// One co-simulated boundary swarm over a seeded step stream.
pub struct CosimSwarm {
    sim: Cosim,
    rng: ChaCha8Rng,
    profile: Profile,
    /// Sessions created, in creation order (a stable index space for picks).
    sessions: Vec<SessionId>,
    report: SwarmReport,
}

impl CosimSwarm {
    pub async fn new(seed: u64, profile: Profile) -> Self {
        // Forked pick stream (offset like the sibling sims so a new world-
        // entropy consumer doesn't shift picks).
        let rng = ChaCha8Rng::seed_from_u64(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        Self {
            sim: Cosim::new(seed).await,
            rng,
            profile,
            sessions: Vec::new(),
            report: SwarmReport {
                seed,
                steps_run: 0,
                sessions_created: 0,
                trace: Vec::new(),
            },
        }
    }

    fn session(&mut self, slot: usize) -> Option<SessionId> {
        if self.sessions.is_empty() {
            return None;
        }
        Some(self.sessions[slot % self.sessions.len()])
    }

    fn pick(&mut self) -> Step {
        let roll: u32 = self.rng.random_range(0..100);
        let slot = self.rng.random_range(0..MAX_SESSIONS);
        // Bias toward BootSession while the world is empty so steps have
        // something to act on (deterministic — a pure function of the world
        // size, not of any I/O).
        if self.sessions.is_empty() {
            return Step::BootSession;
        }
        match self.profile {
            Profile::Calm => match roll {
                0..=6 if self.sessions.len() < MAX_SESSIONS => Step::BootSession,
                7..=20 => Step::GuestWork(slot, self.rng.random_range(1..3)),
                21..=27 => Step::PeriodicCheckpoint(slot),
                28..=36 => Step::EvictToIdle(slot),
                37..=44 => Step::Resume(slot),
                45..=51 => Step::Park(slot),
                52..=58 => Step::Unpause(slot),
                59..=66 => Step::RegisterRehydrate(true),
                67..=70 => Step::StaleSweep,
                71..=78 => Step::ReconcileTick(true),
                79..=82 => Step::DropHostBinding(slot),
                83..=86 => Step::ForceHostLost(slot),
                87..=89 => Step::StragglerSweep,
                90..=91 => Step::FinalizePending,
                92 => Step::SlotClaim,
                93 => Step::SlotPopulate,
                94 => Step::IdleDetector,
                95 => Step::ForceTerminal(slot),
                _ => Step::AdvanceTime(self.rng.random_range(1..4) * 30),
            },
            Profile::Chaos => match roll {
                0..=4 if self.sessions.len() < MAX_SESSIONS => Step::BootSession,
                5..=14 => Step::GuestWork(slot, self.rng.random_range(1..3)),
                15..=20 => Step::PeriodicCheckpoint(slot),
                21..=27 => Step::EvictToIdle(slot),
                28..=33 => Step::Resume(slot),
                34..=40 => Step::Park(slot),
                41..=46 => Step::Unpause(slot),
                47..=53 => Step::Roll,
                54..=61 => Step::RegisterRehydrate(self.rng.random_range(0..4) != 0),
                62..=66 => Step::StaleSweep,
                67..=69 => Step::KillGuest(slot),
                70..=77 => Step::ReconcileTick(true),
                78..=81 => Step::DropHostBinding(slot),
                82..=85 => Step::ForceHostLost(slot),
                86..=88 => Step::StragglerSweep,
                89..=90 => Step::ForceTerminal(slot),
                91..=92 => Step::FinalizePending,
                93 => Step::SlotClaim,
                94 => Step::SlotPopulate,
                95 => Step::IdleDetector,
                // Wave 7b (#784 layer 2): a small-weight record-loss fault so the
                // gap-A family — a resident survivor invisible to the records —
                // arises across the boundary and the barrier's oracles guard it.
                96 => Step::LoseRecord(slot),
                _ => Step::AdvanceTime(self.rng.random_range(1..4) * 30),
            },
        }
    }

    async fn execute(&mut self, step: Step) -> Result<(), String> {
        self.report.trace.push(format!("{step:?}"));
        match step {
            Step::BootSession => {
                let s = self.sim.boot_session().await;
                self.sessions.push(s);
                self.report.sessions_created += 1;
            }
            Step::GuestWork(slot, units) => {
                if let Some(s) = self.session(slot) {
                    self.sim.guest_work(s, units).await;
                }
            }
            Step::PeriodicCheckpoint(slot) => {
                if let Some(s) = self.session(slot) {
                    self.sim.periodic_checkpoint(s).await;
                }
            }
            Step::EvictToIdle(slot) => {
                if let Some(s) = self.session(slot) {
                    if self.sim.session_state(s).await == Some(SessionState::Active) {
                        self.sim.evict_to_idle(s).await;
                    }
                }
            }
            Step::Resume(slot) => {
                if let Some(s) = self.session(slot) {
                    if self.sim.session_state(s).await == Some(SessionState::Idle) {
                        self.sim.resume_session(s).await;
                    }
                }
            }
            Step::Park(slot) => {
                if let Some(s) = self.session(slot) {
                    self.sim.park(s).await;
                }
            }
            Step::Unpause(slot) => {
                if let Some(s) = self.session(slot) {
                    let _ = self.sim.unpause(s).await;
                }
            }
            Step::Roll => {
                // A host-agent roll is a process restart whose STARTUP always
                // rehydrates before the coordinator serves it any traffic
                // (register → rehydrate → serve). Decoupling the two would leave
                // an Active session on a rolled host with no live backend — an
                // unreachable-in-prod state that wedges a later eviction. The
                // survivor-severing WINDOW lives INSIDE register_rehydrate
                // (coord-list → #739 local → stale-sweep), where the directed
                // tests exercise the adversarial missed-survivor variants; the
                // standalone StaleSweep / RegisterRehydrate(false) steps still
                // explore the sweep after the safe startup.
                self.sim.roll_host().await;
                self.sim.register_rehydrate(true).await;
            }
            Step::RegisterRehydrate(local) => self.sim.register_rehydrate(local).await,
            Step::StaleSweep => self.sim.stale_sweep().await,
            Step::KillGuest(slot) => {
                if let Some(s) = self.session(slot) {
                    self.sim.kill_guest(s).await;
                }
            }
            Step::LoseRecord(slot) => {
                if let Some(s) = self.session(slot) {
                    self.sim.lose_record(s).await;
                }
            }
            Step::ReconcileTick(honor) => self.sim.reconcile_tick(honor).await,
            Step::StragglerSweep => self.sim.straggler_sweep_tick().await,
            Step::DropHostBinding(slot) => {
                if let Some(s) = self.session(slot) {
                    if let Some(sb) = self.sim.sandbox_of(s).await {
                        self.sim.drop_local_binding(sb);
                    }
                }
            }
            Step::ForceHostLost(slot) => {
                if let Some(s) = self.session(slot) {
                    if self.sim.session_state(s).await == Some(SessionState::Active) {
                        self.sim
                            .force_session_state(
                                s,
                                SessionState::HostLost,
                                BindingDisposition::Retain,
                            )
                            .await;
                    }
                }
            }
            Step::ForceTerminal(slot) => {
                if let Some(s) = self.session(slot) {
                    self.sim
                        .force_session_state(s, SessionState::Failed, BindingDisposition::Detach)
                        .await;
                }
            }
            Step::FinalizePending => self.sim.finalize_pending().await,
            Step::SlotClaim => self.sim.slot_claim().await,
            Step::SlotPopulate => self.sim.slot_populate_tick().await,
            Step::IdleDetector => self.sim.idle_detector().await,
            Step::AdvanceTime(secs) => self.sim.advance(secs).await,
        }
        Ok(())
    }

    /// Run `steps` picks, checking the standing oracles after every step, then
    /// quiesce and require convergence.
    pub async fn run(&mut self, steps: u64) -> Result<SwarmReport, String> {
        for _ in 0..steps {
            let step = self.pick();
            self.execute(step).await?;
            self.report.steps_run += 1;
            self.check_oracles().await?;
        }
        self.quiesce().await?;
        let seed = self.report.seed;
        Ok(std::mem::replace(
            &mut self.report,
            SwarmReport {
                seed,
                steps_run: 0,
                sessions_created: 0,
                trace: Vec::new(),
            },
        ))
    }

    /// The every-step standing oracles (deliverable 3): severed-live-holder
    /// (#806), no-plane-leak (slot-accounting), and the cross-boundary
    /// ACTIVE-serve ownership split-brain guard. The idle⇒durable-snapshot
    /// (#570) oracle is asserted at QUIESCENCE instead: the D5 fast path marks
    /// Idle BEFORE the async finalize lands the durable snapshot row (a real
    /// production window — the heartbeat reconcile lands it), so the property is
    /// "the COMPLETED eviction is durable," which #570's own directed test
    /// checks post-finalize. A cancelled finalize (the #570 loss) still lands NO
    /// snapshot by quiescence, so the detection is preserved.
    async fn check_oracles(&self) -> Result<(), String> {
        self.sim.assert_no_severed_live_holder().await?;
        self.sim.assert_quarantine_reconnectable().await?;
        self.sim.assert_slot_accounting().await?;
        self.sim.assert_ownership_agreement().await?;
        Ok(())
    }

    /// Quiesce: heal every fault (re-serve survivors, drain finalizes, drive
    /// reconcile + the straggler sweep to a fixed point), then require
    /// convergence — every session terminal-or-stable, every device classified,
    /// within bounded rounds — and re-check the standing oracles.
    async fn quiesce(&mut self) -> Result<(), String> {
        // Wave 7b (#784): heal the record-loss fault globally (the operator/runbook
        // reconcile the `rehydrate-unknown-device` alert drives, applied to every
        // survivor at quiescence like every other healed fault) so the rehydrate
        // below re-serves every survivor. A quarantined slot MUST reach a terminal
        // disposition (served or reaped) within bounded rounds, never wedge.
        self.sim.heal_all_records().await;
        // A fresh host generation with a clean rehydrate re-serves every
        // survivor whose session still reserves host memory.
        self.sim.register_rehydrate(true).await;
        // Bounded convergence drain: reconcile repairs/reaps, the straggler
        // sweep settles HostLost rows (coarse time crosses the 60 s min-age),
        // and finalizes drain.
        for _ in 0..(MAX_SESSIONS as u64 + 6) {
            self.sim.advance(120).await;
            // Drive any pending coordinator op (a backed-off Evict/Resume) to a
            // terminal row state, then the host-side legs.
            self.sim.drive_ops().await;
            self.sim.reconcile_tick(true).await;
            self.sim.straggler_sweep_tick().await;
            self.sim.finalize_pending().await;
            self.check_oracles().await?;
        }
        // Convergence: no session may still be mid-eviction (Evicting) or wedged
        // at HostLost after the drain — every one is terminal-or-stable.
        for &s in &self.sessions {
            match self.sim.session_state(s).await {
                Some(SessionState::Evicting) => {
                    return Err(format!(
                        "convergence: session {s} still Evicting at quiescence — the eviction \
                         pipeline never settled (a stuck capture/finalize)"
                    ));
                }
                Some(SessionState::HostLost) => {
                    return Err(format!(
                        "convergence: session {s} still HostLost at quiescence — the straggler \
                         sweep never settled it (the #762/#769 eternal-wedge class)"
                    ));
                }
                _ => {}
            }
        }
        self.check_oracles().await?;
        // Teardown/ownership completeness: every served device must be
        // coordinator-owned by quiescence (the reconcile/finalize drain
        // converged every leftover).
        self.sim.assert_teardown_complete().await?;
        // #570 (idle⇒durable-snapshot): the finalize drain has completed every
        // in-flight eviction capture, so each evicted session must now have a
        // recoverable snapshot at-or-above its eviction cursor. A finalize the
        // reconcile cancelled (the #570 loss) lands none and fires here.
        for &s in &self.sessions {
            self.sim.assert_idle_snapshot_durable(s)?;
        }
        Ok(())
    }

    pub fn report(&self) -> &SwarmReport {
        &self.report
    }
}

/// Run ONE seed on its OWN current-thread paused-clock runtime, DROPPED at the
/// boundary (the `engram-dst` `run_seed` shape — determinism-audit item 8: no
/// detached task leaks across a seed boundary). The CLI adds the named 600 s
/// watchdog over this call.
pub fn run_seed(seed: u64, profile: Profile, steps: u64, trace_tail: usize) -> SeedOutcome {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("current-thread runtime");
    let outcome = rt.block_on(async move {
        tokio::time::pause();
        let mut swarm = CosimSwarm::new(seed, profile).await;
        let result = swarm.run(steps).await;
        let trace = swarm
            .report()
            .trace
            .iter()
            .rev()
            .take(trace_tail)
            .rev()
            .cloned()
            .collect::<Vec<_>>();
        SeedOutcome {
            seed,
            result,
            trace,
        }
    });
    drop(rt);
    outcome
}

/// The outcome of running ONE seed to a verdict.
pub struct SeedOutcome {
    pub seed: u64,
    pub result: Result<SwarmReport, String>,
    pub trace: Vec<String>,
}
