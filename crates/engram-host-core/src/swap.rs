//! ADR 0112 D3: the pre-capture swap-disarm DECISION, pure.
//!
//! The invariant it protects: **no restorable memory image ever
//! contains swap PTEs.** A restored image with live swap state is
//! paired with a fresh zero-filled backing file, so its swap entries
//! point at bytes that no longer exist — silent memory corruption,
//! worse than ADR 0028 Defect B (no filesystem journal stands between
//! the kernel and the damage). The disarm (`swapoff -a`) pages used
//! swap back into RAM, which is unbounded — so the decision half is
//! guarded, and the guards live here where unit tests and the host
//! simulator can drive them (ADR 0098: keep the pure step out of the
//! loop).
//!
//! `PooledBackend::swap_disarm` executes the verdict; this module only
//! decides it.

/// Kibibytes of headroom `swapoff` must have on top of used swap —
/// absorbs usage growth between the meminfo probe and the swapoff
/// (TOCTOU) and keeps the page-back-in from starving the guest into
/// the OOM killer mid-disarm. Applied through [`disarm_margin_kb`],
/// which bounds it to a quarter of `MemAvailable`: the flat figure
/// exceeded the ENTIRE MemAvailable of a small guest (CI's 256 MiB
/// fixtures), making every capture refuse at zero used swap — caught
/// by the `swap_capture` FC lane.
pub const SWAP_DISARM_MARGIN_KB: u64 = 256 * 1024;

/// The effective disarm margin for a guest with this much
/// `MemAvailable`: the flat [`SWAP_DISARM_MARGIN_KB`] on real-sized
/// guests, a quarter of available on small ones — proportional
/// headroom, never a guard that can exceed the whole budget it
/// protects.
pub fn disarm_margin_kb(mem_available_kb: u64) -> u64 {
    SWAP_DISARM_MARGIN_KB.min(mem_available_kb / 4)
}

/// The periodic-checkpoint flavor's used-swap ceiling. ADR 0101 ticks
/// come as fast as every 30 s; paging more than this back per tick,
/// only for the kernel to re-evict it, is pure thrash — and the
/// continuously-flushed disk already IS that tick's durability
/// (recovery = the existing rung-2 cold disk boot).
pub const PERIODIC_MAX_SWAP_USED_KB: u64 = 256 * 1024;

/// Which capture flavor is asking — the flavors carry different
/// refusal thresholds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapDisarmPolicy {
    /// Eviction, drain, operator snapshot, base bake: disarm at any
    /// swap usage that fits back into RAM. Terminal captures target
    /// idle sessions, whose `MemAvailable` is high — a refusal here
    /// means the guest is actively thrashing, and the eviction op
    /// requeues (sustained refusal escalates through the existing
    /// quarantine ladder to destroy + rung-2 recovery, which is the
    /// disk-only eviction by another road).
    Terminal,
    /// ADR 0101 periodic checkpoints: additionally refuse above
    /// [`PERIODIC_MAX_SWAP_USED_KB`].
    Periodic,
}

impl SwapDisarmPolicy {
    /// Metric label (`engram_swap_disarm_refused_total{flavor=…}`).
    pub fn label(self) -> &'static str {
        match self {
            SwapDisarmPolicy::Terminal => "terminal",
            SwapDisarmPolicy::Periodic => "periodic",
        }
    }
}

/// The verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapDisarmPlan {
    /// No swap area is active (device never armed / kill switch):
    /// nothing to disarm, nothing to re-arm, capture proceeds.
    NoSwap,
    /// Run `swapoff -a`, then capture, then re-arm. `used_kb` is
    /// carried for logging/metrics.
    Disarm { used_kb: u64 },
    /// Do NOT capture memory: `swapoff` can't complete safely.
    /// Nothing must be consumed (chain head, dirty bitmap) — the
    /// caller surfaces a typed error and the capture retries or
    /// degrades per flavor.
    Refuse {
        used_kb: u64,
        mem_available_kb: u64,
        reason: SwapRefuseReason,
    },
}

/// Why a disarm was refused — split so metrics/logs distinguish "the
/// guest cannot absorb its swap" from "the periodic ceiling said not
/// worth it this tick".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapRefuseReason {
    /// `SwapUsed + margin > MemAvailable`: the page-back-in would not
    /// fit; forcing it risks ENOMEM/OOM inside the guest.
    WontFit,
    /// Periodic flavor above [`PERIODIC_MAX_SWAP_USED_KB`].
    PeriodicCeiling,
}

impl SwapRefuseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SwapRefuseReason::WontFit => "swapoff would not fit back into RAM",
            SwapRefuseReason::PeriodicCeiling => "used swap above the periodic-checkpoint ceiling",
        }
    }
}

/// Decide the disarm arm from the guest's `/proc/meminfo` numbers
/// (all in kB, as the kernel reports them).
pub fn plan_swap_disarm(
    swap_total_kb: u64,
    swap_free_kb: u64,
    mem_available_kb: u64,
    policy: SwapDisarmPolicy,
) -> SwapDisarmPlan {
    if swap_total_kb == 0 {
        return SwapDisarmPlan::NoSwap;
    }
    let used_kb = swap_total_kb.saturating_sub(swap_free_kb);
    if used_kb + disarm_margin_kb(mem_available_kb) > mem_available_kb {
        return SwapDisarmPlan::Refuse {
            used_kb,
            mem_available_kb,
            reason: SwapRefuseReason::WontFit,
        };
    }
    if policy == SwapDisarmPolicy::Periodic && used_kb > PERIODIC_MAX_SWAP_USED_KB {
        return SwapDisarmPlan::Refuse {
            used_kb,
            mem_available_kb,
            reason: SwapRefuseReason::PeriodicCeiling,
        };
    }
    SwapDisarmPlan::Disarm { used_kb }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB_KB: u64 = 1024 * 1024;

    #[test]
    fn no_swap_area_skips() {
        assert_eq!(
            plan_swap_disarm(0, 0, 4 * GIB_KB, SwapDisarmPolicy::Terminal),
            SwapDisarmPlan::NoSwap,
        );
    }

    #[test]
    fn healthy_guest_disarms_both_flavors() {
        // 6 GiB device, 0 used, plenty available — the steady state.
        for policy in [SwapDisarmPolicy::Terminal, SwapDisarmPolicy::Periodic] {
            assert_eq!(
                plan_swap_disarm(6 * GIB_KB, 6 * GIB_KB, 8 * GIB_KB, policy),
                SwapDisarmPlan::Disarm { used_kb: 0 },
            );
        }
    }

    #[test]
    fn wont_fit_refuses_both_flavors() {
        // 4 GiB out, 2 GiB available: paging back cannot fit.
        let used = 4 * GIB_KB;
        for policy in [SwapDisarmPolicy::Terminal, SwapDisarmPolicy::Periodic] {
            match plan_swap_disarm(6 * GIB_KB, 2 * GIB_KB, 2 * GIB_KB, policy) {
                SwapDisarmPlan::Refuse {
                    used_kb,
                    reason: SwapRefuseReason::WontFit,
                    ..
                } => assert_eq!(used_kb, used),
                other => panic!("expected WontFit refusal, got {other:?}"),
            }
        }
    }

    #[test]
    fn small_guest_with_no_used_swap_disarms() {
        // The CI-caught regression, exact shape: a 256 MiB guest
        // (64 MiB swap device, ~192 MiB MemAvailable, zero used). The
        // flat 256 MiB margin exceeded the WHOLE MemAvailable, so
        // every capture refused as "won't fit" with nothing to fit.
        let plan = plan_swap_disarm(65_536, 65_536, 196_748, SwapDisarmPolicy::Periodic);
        assert_eq!(plan, SwapDisarmPlan::Disarm { used_kb: 0 }, "{plan:?}");
        // And with real usage on the same small guest: 64 MiB paging
        // back into ~192 MiB available fits comfortably.
        let plan = plan_swap_disarm(65_536, 0, 196_748, SwapDisarmPolicy::Terminal);
        assert_eq!(plan, SwapDisarmPlan::Disarm { used_kb: 65_536 }, "{plan:?}");
        // The proportional margin still refuses when the page-back-in
        // genuinely crowds MemAvailable.
        let plan = plan_swap_disarm(65_536, 0, 70_000, SwapDisarmPolicy::Terminal);
        assert!(
            matches!(
                plan,
                SwapDisarmPlan::Refuse {
                    reason: SwapRefuseReason::WontFit,
                    ..
                }
            ),
            "{plan:?}",
        );
    }

    #[test]
    fn margin_is_enforced_not_just_equality() {
        // used == available: the margin must still refuse.
        let plan = plan_swap_disarm(2 * GIB_KB, GIB_KB, GIB_KB, SwapDisarmPolicy::Terminal);
        assert!(matches!(plan, SwapDisarmPlan::Refuse { .. }), "{plan:?}");
        // used + margin just fits: disarm.
        let plan = plan_swap_disarm(
            2 * GIB_KB,
            GIB_KB,
            GIB_KB + SWAP_DISARM_MARGIN_KB + 1,
            SwapDisarmPolicy::Terminal,
        );
        assert!(matches!(plan, SwapDisarmPlan::Disarm { .. }), "{plan:?}");
    }

    #[test]
    fn periodic_ceiling_refuses_where_terminal_disarms() {
        // 1 GiB out of a fits-fine guest: terminal pays it (final
        // capture), periodic declines (30 s ticks).
        let (total, free, avail) = (6 * GIB_KB, 5 * GIB_KB, 8 * GIB_KB);
        assert_eq!(
            plan_swap_disarm(total, free, avail, SwapDisarmPolicy::Terminal),
            SwapDisarmPlan::Disarm { used_kb: GIB_KB },
        );
        match plan_swap_disarm(total, free, avail, SwapDisarmPolicy::Periodic) {
            SwapDisarmPlan::Refuse {
                reason: SwapRefuseReason::PeriodicCeiling,
                ..
            } => {}
            other => panic!("expected PeriodicCeiling refusal, got {other:?}"),
        }
    }
}
