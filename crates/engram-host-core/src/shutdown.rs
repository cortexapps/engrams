//! The SIGTERM shutdown-ladder DECISIONS (ADR 0098 Phase 2, Flow A).
//!
//! When a host-agent pod receives SIGTERM it walks a **linear ladder**:
//! abort the background tasks, run a bounded final disk-flush over the
//! surviving NBD data planes, abandon those planes (leaving the kernel-side
//! devices alive for the successor), export any still-un-uploaded dirty tier
//! to the node-local shutdown spool, and detach the microVMs. The 2026-07-16
//! session-85e0298a acked-write-loss corruption lived in exactly this path.
//!
//! This module holds the **pure decisions** the ladder makes, extracted out
//! of the driver so the host-internal simulator (`engram-dst-host`) drives
//! the real ordering/classification on macOS and a seeded crash injector can
//! reason about them. It lives in `engram-host-core` — not the host-agent —
//! precisely because these functions touch only `std` types (`Duration`,
//! `usize`, `u64`, `bool`): they are cleanly portable, so the honest home is
//! the portable crate (contrast Flow C's `classify`, which is coupled to the
//! host-agent's `ReconcileBackend` and stays host-agent-local).
//!
//! **What stays in the driver (concurrency gates, NOT decisions):** the
//! `abandoning` SeqCst flag raise-first ordering, the drain-twice sweep over
//! `nbd_sandboxes`, the per-survivor `tokio::spawn` fan-out + the
//! `tokio::time::timeout` deadline, the `spawn_blocking` device sync (now via
//! [`DeviceSync::sync_device`](crate::DeviceSync)), and
//! `NbdSandboxState::abandon_for_shutdown`'s ownership-consuming semantics.
//! Those are #224's correctness gates; this module owns only what to DECIDE,
//! never how to sequence the effects.

use std::time::Duration;

/// Default final-flush budget (seconds) when `ENGRAM_SHUTDOWN_FLUSH_BUDGET_SECS`
/// is unset, unparseable, or non-positive. Budgeted against the pod's
/// `terminationGracePeriodSeconds` minus headroom for the abandon sweep +
/// detach.
pub const DEFAULT_FLUSH_BUDGET_SECS: f64 = 20.0;

/// The SIGTERM ladder's stages, in ladder order. Declaration order **is** the
/// progression order (the derived [`Ord`] compares by it), so the ordering is
/// auditable: `Signaled < TasksAborted < FinalFlush < Abandon < SpoolExport <
/// Detached`. The ladder is strictly linear — there are no branches.
///
/// The enum is wildcard-free at its use sites and has an explicit [`LADDER`]
/// array, so a new stage is a compile error rather than a silent gap.
///
/// [`LADDER`]: ShutdownStage::LADDER
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShutdownStage {
    /// SIGTERM received; the shutdown handler has begun.
    Signaled,
    /// The background tasks (heartbeat, gRPC, registration/rehydrate) are
    /// aborted — the largest insert-after-sweep source is removed (#224).
    TasksAborted,
    /// The bounded final disk-flush pass over the surviving NBD data planes.
    FinalFlush,
    /// The terminal abandon sweep: raise `abandoning`, drain-twice, leave the
    /// kernel devices alive for the successor.
    Abandon,
    /// Export every still-un-uploaded dirty tier to the node-local shutdown
    /// spool (the 85e0298a store-ahead leg).
    SpoolExport,
    /// Detach the microVMs (left alive for the successor to pidfd-reattach);
    /// the process exits.
    Detached,
}

impl ShutdownStage {
    /// Every stage, in ladder order. A new [`ShutdownStage`] variant that is
    /// not added here is a compile error at the array literal.
    pub const LADDER: [ShutdownStage; 6] = [
        ShutdownStage::Signaled,
        ShutdownStage::TasksAborted,
        ShutdownStage::FinalFlush,
        ShutdownStage::Abandon,
        ShutdownStage::SpoolExport,
        ShutdownStage::Detached,
    ];

    /// The next stage in the linear ladder, or `None` at the terminal
    /// [`Detached`](ShutdownStage::Detached).
    pub fn next(self) -> Option<ShutdownStage> {
        let i = Self::LADDER.iter().position(|s| *s == self)?;
        Self::LADDER.get(i + 1).copied()
    }

    /// Legality of a stage transition: the ladder only ever advances by one
    /// step. `from.can_advance_to(to)` iff `to` is `from`'s immediate
    /// successor — the driver walks the ladder in exactly this order, so a
    /// skip or a repeat is a bug.
    pub fn can_advance_to(self, to: ShutdownStage) -> bool {
        self.next() == Some(to)
    }
}

/// Issue #224: once the ladder reaches [`Abandon`](ShutdownStage::Abandon) the
/// terminal `abandoning` flag is raised, and NO new live NBD data plane may be
/// inserted into `nbd_sandboxes` — a late insert (a `create`/`rehydrate`
/// completing in its multi-second await window) must
/// `abandon_for_shutdown()` in-place instead, so process exit's
/// `NbdHandle::Drop` never netlink-disconnects a survivor's device the
/// successor is about to RECONFIGURE. This predicate is the pure ordering
/// gate; the concrete SeqCst flag + drain-twice sweep are the Linux-bound
/// concurrency implementation in the driver.
pub fn admits_new_plane(stage: ShutdownStage) -> bool {
    stage < ShutdownStage::Abandon
}

/// The final-flush deadline the driver budgets its per-survivor fan-out
/// against, from a pre-parsed env value. Non-positive / absent ⇒ the default.
/// Pure so the parse-and-default is unit-tested away from the driver's
/// `std::env::var` read.
pub fn flush_budget(env_value: Option<f64>) -> Duration {
    let secs = env_value
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_FLUSH_BUDGET_SECS);
    Duration::from_secs_f64(secs)
}

/// What the final-flush pass observed for one survivor, BEFORE the coord
/// publish executes — the input to [`classify_survivor`]. The `bound` flag is
/// "a coord publisher is wired AND the sandbox is bound to a session" (an
/// unbound / no-coord survivor's chunks are durable in the store but cannot be
/// published from here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushProbe {
    /// `backend.flush()` errored — nothing uploaded this pass.
    FlushError,
    /// `backend.flush()` returned `Ok`: `chunks_flushed` chunks uploaded.
    Flushed { chunks_flushed: usize, bound: bool },
}

/// The per-survivor action the final-flush fan-out takes. This is the decision
/// content that used to be interleaved inline in
/// `flush_nbd_data_planes_for_shutdown`; the driver now sequences the effects
/// off this verdict (exactly like Flow C's `reconcile_once` acts off
/// `classify`'s `ReapVerdict`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurvivorAction {
    /// `chunks_flushed == 0` — the survivor was already clean; skip the
    /// publish (there is nothing new to point coord at).
    SkipClean,
    /// The flush errored — the survivor's un-uploaded writes ride the shutdown
    /// spool to the successor (the abandon-sweep spool export is the
    /// unconditional backstop). The same posture covers a *publish* that fails
    /// after a successful upload: the spool's store-ahead ref still points the
    /// successor at the just-uploaded chunks.
    RelyOnSpool,
    /// Chunks were uploaded and are durable in the store, but the survivor is
    /// unbound / no coord publisher is wired — durable in GCS, no publish.
    DurableNoPublish,
    /// Chunks were uploaded and the survivor is bound — publish the new
    /// `live_disk_manifest` so the successor rehydrates from the current ref.
    Publish,
}

/// The pure final-flush decision core: map one survivor's flush probe to the
/// action. Every arm preserves the pre-extraction inline semantics.
pub fn classify_survivor(probe: FlushProbe) -> SurvivorAction {
    match probe {
        FlushProbe::FlushError => SurvivorAction::RelyOnSpool,
        FlushProbe::Flushed {
            chunks_flushed: 0, ..
        } => SurvivorAction::SkipClean,
        FlushProbe::Flushed { bound: false, .. } => SurvivorAction::DurableNoPublish,
        FlushProbe::Flushed { .. } => SurvivorAction::Publish,
    }
}

/// The deadline-overrun straggler check: a survivor still holding
/// un-uploaded dirty bytes when the flush deadline fires is a loud straggler
/// — no longer a data-loss event (the abandon-sweep spool export catches it),
/// but the GCS-side durability gap on this node stays visible. `true` ⇒ log
/// loudly.
pub fn is_straggler(dirty_bytes: u64) -> bool {
    dirty_bytes > 0
}

/// The auditable shutdown plan the driver executes: the deadline the
/// final-flush fan-out is budgeted against, plus the (constant, linear)
/// stage ladder. The survivor set is *not* part of the plan — the fan-out
/// runs over every entry in `nbd_sandboxes` with no filtering, so the only
/// arithmetic here is the deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownPlan {
    /// The final-flush deadline (from the flush budget).
    pub flush_deadline: Duration,
}

impl ShutdownPlan {
    /// The ladder this plan is walked through, in order.
    pub const LADDER: [ShutdownStage; 6] = ShutdownStage::LADDER;
}

/// Build the shutdown plan from the pre-parsed budget env value.
pub fn plan_shutdown(env_value: Option<f64>) -> ShutdownPlan {
    ShutdownPlan {
        flush_deadline: flush_budget(env_value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_budget_defaults_on_absent_or_non_positive() {
        // Absent → default.
        assert_eq!(
            flush_budget(None),
            Duration::from_secs_f64(DEFAULT_FLUSH_BUDGET_SECS)
        );
        // Zero and negative are non-positive → default (mirrors the driver's
        // `filter(|v| *v > 0.0)`).
        assert_eq!(
            flush_budget(Some(0.0)),
            Duration::from_secs_f64(DEFAULT_FLUSH_BUDGET_SECS)
        );
        assert_eq!(
            flush_budget(Some(-5.0)),
            Duration::from_secs_f64(DEFAULT_FLUSH_BUDGET_SECS)
        );
        // A positive value is honored verbatim (incl. sub-second, the #225
        // overrun case).
        assert_eq!(flush_budget(Some(7.5)), Duration::from_secs_f64(7.5));
        assert_eq!(flush_budget(Some(0.001)), Duration::from_secs_f64(0.001));
    }

    #[test]
    fn plan_shutdown_carries_the_budget_and_the_linear_ladder() {
        assert_eq!(
            plan_shutdown(Some(3.0)).flush_deadline,
            Duration::from_secs(3)
        );
        assert_eq!(ShutdownPlan::LADDER, ShutdownStage::LADDER);
    }

    #[test]
    fn ladder_is_linear_and_ordered() {
        // The declared array is exactly the derived-Ord order.
        let mut sorted = ShutdownStage::LADDER;
        sorted.sort();
        assert_eq!(sorted, ShutdownStage::LADDER);
        // `next()` walks it, terminal at Detached.
        assert_eq!(
            ShutdownStage::Signaled.next(),
            Some(ShutdownStage::TasksAborted)
        );
        assert_eq!(
            ShutdownStage::SpoolExport.next(),
            Some(ShutdownStage::Detached)
        );
        assert_eq!(ShutdownStage::Detached.next(), None);
        // `can_advance_to` only permits the immediate successor.
        assert!(ShutdownStage::FinalFlush.can_advance_to(ShutdownStage::Abandon));
        assert!(!ShutdownStage::FinalFlush.can_advance_to(ShutdownStage::SpoolExport));
        assert!(!ShutdownStage::FinalFlush.can_advance_to(ShutdownStage::FinalFlush));
    }

    #[test]
    fn abandon_stage_closes_the_new_plane_gate() {
        // #224: before Abandon, inserts are admitted; from Abandon on, a late
        // insert must abandon-in-place.
        assert!(admits_new_plane(ShutdownStage::Signaled));
        assert!(admits_new_plane(ShutdownStage::TasksAborted));
        assert!(admits_new_plane(ShutdownStage::FinalFlush));
        assert!(!admits_new_plane(ShutdownStage::Abandon));
        assert!(!admits_new_plane(ShutdownStage::SpoolExport));
        assert!(!admits_new_plane(ShutdownStage::Detached));
    }

    #[test]
    fn classify_survivor_truth_table() {
        // flush errored → the spool store-ahead is the durability path.
        assert_eq!(
            classify_survivor(FlushProbe::FlushError),
            SurvivorAction::RelyOnSpool
        );
        // zero chunks → already clean, skip publish.
        assert_eq!(
            classify_survivor(FlushProbe::Flushed {
                chunks_flushed: 0,
                bound: true,
            }),
            SurvivorAction::SkipClean
        );
        // zero chunks wins even when unbound.
        assert_eq!(
            classify_survivor(FlushProbe::Flushed {
                chunks_flushed: 0,
                bound: false,
            }),
            SurvivorAction::SkipClean
        );
        // uploaded but unbound → durable in the store, no publish.
        assert_eq!(
            classify_survivor(FlushProbe::Flushed {
                chunks_flushed: 4,
                bound: false,
            }),
            SurvivorAction::DurableNoPublish
        );
        // uploaded + bound → publish the new live manifest.
        assert_eq!(
            classify_survivor(FlushProbe::Flushed {
                chunks_flushed: 4,
                bound: true,
            }),
            SurvivorAction::Publish
        );
    }

    #[test]
    fn straggler_iff_dirty_bytes_remain() {
        assert!(!is_straggler(0));
        assert!(is_straggler(1));
        assert!(is_straggler(1 << 20));
    }
}
