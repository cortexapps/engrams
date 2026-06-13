//! ADR 0048: the scale-down WAVE planner — pure, deterministic, fully
//! unit-tested.
//!
//! [`desired_hosts`](crate::scaler::desired_hosts) computes the cost-optimal
//! target (how SMALL the pool could be). This planner turns that into the
//! concrete set of victim nodes to drain + remove THIS reconcile, bounded by:
//!
//! - the per-wave shed cap (`max_shed_per_wave`) — small waves, observable;
//! - the floor (`max(min_hosts, capacity_floor)`) — never strand the fleet;
//! - a **2D capacity guard** — the victims' reserved RAM *and* CPU must fit on
//!   the survivors (with RAM headroom) before we start a single move, so a
//!   teleport can't strand a session on a full fleet (the operator-side mirror
//!   of the coordinator's per-session don't-strand guard).
//!
//! Victim selection: `IdleOnly` considers only empty hosts; `Aggressive`
//! considers all and prefers the least-loaded (`(running_sandboxes,
//! reserved_mib)` ascending). Already-`pinned` victims (a wave the operator
//! annotated before a restart) sort FIRST so a resumed wave finishes what it
//! started instead of re-picking a different set.

use crate::scaler::ScaleDownMode;

/// One host as the wave planner sees it: its K8s node name (what we cordon +
/// remove) plus the load + budget metrics from the coordinator's host view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaveHost {
    /// K8s node name (== GCE instance name on GKE) — the cordon + remove key.
    pub node: String,
    pub running_sandboxes: u32,
    /// Σ reserved by this host's sessions (what must move if it's drained).
    pub reserved_mib: u64,
    pub reserved_vcpus: u64,
    /// Free budget on this host (what it can ABSORB as a survivor).
    pub free_mib: u64,
    pub free_vcpus: u64,
    /// Already annotated as a victim of an in-flight wave (resume-after-restart
    /// finishes these first).
    pub pinned: bool,
}

/// Wave bounds from the CR + autoscaling spec.
#[derive(Clone, Copy, Debug)]
pub struct WavePolicy {
    pub mode: ScaleDownMode,
    /// Most victims to start draining in a single reconcile.
    pub max_shed_per_wave: u32,
    /// `max(min_hosts, capacity_floor)` — never shed below this many hosts.
    pub floor: u32,
    /// RAM (MiB) of survivor headroom to preserve after the wave's sessions
    /// land (the same `target_free_mib` the scale-up policy targets).
    pub headroom_mib: u64,
}

/// The plan for one reconcile: which nodes to cordon + drain + remove, and a
/// human-readable note for logs/metrics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaveDecision {
    pub victims: Vec<String>,
    pub note: String,
}

impl WaveDecision {
    fn none(note: impl Into<String>) -> Self {
        Self {
            victims: Vec::new(),
            note: note.into(),
        }
    }
}

/// Plan the next wave. `desired` is the cost-optimal target from
/// `desired_hosts`; `hosts` is every host currently in the pool (schedulable
/// ones plus any already-pinned in-flight victims). Deterministic.
pub fn plan_wave(hosts: &[WaveHost], desired: u32, policy: WavePolicy) -> WaveDecision {
    if policy.mode == ScaleDownMode::Off {
        return WaveDecision::none("scale-down off");
    }
    let current = hosts.len() as u32;

    // How many we may shed: bounded by the target, the per-wave cap, and the
    // floor.
    let want = current.saturating_sub(desired);
    let floor_room = current.saturating_sub(policy.floor);
    let budget = want.min(policy.max_shed_per_wave).min(floor_room);
    if budget == 0 {
        return WaveDecision::none(format!(
            "nothing to shed (current={current}, desired={desired}, \
             floor={}, max_per_wave={})",
            policy.floor, policy.max_shed_per_wave
        ));
    }

    // Eligible victims by mode.
    let mut candidates: Vec<&WaveHost> = hosts
        .iter()
        .filter(|h| match policy.mode {
            ScaleDownMode::Off => false,
            ScaleDownMode::IdleOnly => h.running_sandboxes == 0,
            ScaleDownMode::Aggressive => true,
        })
        .collect();

    // Order: pinned (in-flight) first, then least-loaded, then node name for a
    // deterministic tie-break.
    candidates.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then(a.running_sandboxes.cmp(&b.running_sandboxes))
            .then(a.reserved_mib.cmp(&b.reserved_mib))
            .then(a.node.cmp(&b.node))
    });
    candidates.truncate(budget as usize);

    // 2D capacity guard: the chosen victims' reserved RAM + CPU must fit on
    // the survivors (every host NOT chosen), keeping RAM headroom. If it
    // doesn't, drop the most-loaded victim (the tail of the least-loaded
    // ordering) and re-check — shed as many as safely fit, never start a move
    // that would strand a session.
    let mut trimmed = false;
    while !candidates.is_empty() {
        let victim_nodes: std::collections::HashSet<&str> =
            candidates.iter().map(|h| h.node.as_str()).collect();
        let (v_mib, v_vcpus) = candidates.iter().fold((0u64, 0u64), |(m, c), h| {
            (m + h.reserved_mib, c + h.reserved_vcpus)
        });
        let (s_free_mib, s_free_vcpus) = hosts
            .iter()
            .filter(|h| !victim_nodes.contains(h.node.as_str()))
            .fold((0u64, 0u64), |(m, c), h| (m + h.free_mib, c + h.free_vcpus));
        if v_mib + policy.headroom_mib <= s_free_mib && v_vcpus <= s_free_vcpus {
            break;
        }
        candidates.pop(); // drop the most-loaded victim, re-check
        trimmed = true;
    }

    if candidates.is_empty() {
        return WaveDecision::none(if trimmed {
            "no victim fits on survivors (2D guard) — scale up before shedding"
        } else {
            "no eligible victims"
        });
    }

    let victims: Vec<String> = candidates.iter().map(|h| h.node.clone()).collect();
    let note = format!(
        "shedding {} (budget={budget}, mode={:?}{})",
        victims.len(),
        policy.mode,
        if trimmed { ", 2D-guard-trimmed" } else { "" }
    );
    WaveDecision { victims, note }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(node: &str, running: u32, reserved_mib: u64, free_mib: u64) -> WaveHost {
        WaveHost {
            node: node.into(),
            running_sandboxes: running,
            reserved_mib,
            reserved_vcpus: 0,
            free_mib,
            free_vcpus: 1_000_000, // CPU not the binding dim in most tests
            pinned: false,
        }
    }

    fn policy(mode: ScaleDownMode, max_per_wave: u32, floor: u32, headroom: u64) -> WavePolicy {
        WavePolicy {
            mode,
            max_shed_per_wave: max_per_wave,
            floor,
            headroom_mib: headroom,
        }
    }

    #[test]
    fn off_mode_sheds_nothing() {
        let hosts = vec![host("a", 0, 0, 16_000), host("b", 0, 0, 16_000)];
        let d = plan_wave(&hosts, 1, policy(ScaleDownMode::Off, 4, 1, 0));
        assert!(d.victims.is_empty());
    }

    #[test]
    fn nothing_to_shed_when_at_target() {
        let hosts = vec![host("a", 0, 0, 16_000), host("b", 0, 0, 16_000)];
        let d = plan_wave(&hosts, 2, policy(ScaleDownMode::Aggressive, 4, 1, 0));
        assert!(d.victims.is_empty(), "current==desired → no shed");
    }

    #[test]
    fn shed_is_clamped_by_max_per_wave() {
        // 5 idle hosts, desired 1 (want 4), but max_per_wave 2 → shed 2.
        let hosts: Vec<WaveHost> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|n| host(n, 0, 0, 16_000))
            .collect();
        let d = plan_wave(&hosts, 1, policy(ScaleDownMode::Aggressive, 2, 1, 0));
        assert_eq!(d.victims.len(), 2);
    }

    #[test]
    fn shed_is_clamped_by_floor() {
        // 3 idle hosts, desired 0, max_per_wave 10, floor 2 → may shed only 1.
        let hosts: Vec<WaveHost> = ["a", "b", "c"]
            .iter()
            .map(|n| host(n, 0, 0, 16_000))
            .collect();
        let d = plan_wave(&hosts, 0, policy(ScaleDownMode::Aggressive, 10, 2, 0));
        assert_eq!(d.victims.len(), 1);
    }

    #[test]
    fn idle_only_skips_loaded_hosts() {
        // Only the empty host is eligible even though desired says shed 2.
        let hosts = vec![host("loaded", 3, 8_000, 8_000), host("empty", 0, 0, 16_000)];
        let d = plan_wave(&hosts, 0, policy(ScaleDownMode::IdleOnly, 4, 0, 0));
        assert_eq!(d.victims, vec!["empty".to_string()]);
    }

    #[test]
    fn aggressive_picks_least_loaded_first() {
        // Three hosts, shed 1 → the least-loaded (fewest sandboxes) goes.
        let hosts = vec![
            host("busy", 5, 10_000, 6_000),
            host("quiet", 1, 2_000, 14_000),
            host("mid", 3, 6_000, 10_000),
        ];
        let d = plan_wave(&hosts, 2, policy(ScaleDownMode::Aggressive, 1, 0, 0));
        assert_eq!(d.victims, vec!["quiet".to_string()]);
    }

    #[test]
    fn pinned_victims_sort_first() {
        // A pinned (in-flight) victim is re-selected first even though it's
        // more loaded than an idle peer — a resumed wave finishes it.
        let mut pinned = host("pinned", 4, 8_000, 8_000);
        pinned.pinned = true;
        let hosts = vec![host("idle", 0, 0, 16_000), pinned];
        let d = plan_wave(&hosts, 1, policy(ScaleDownMode::Aggressive, 1, 0, 0));
        assert_eq!(d.victims, vec!["pinned".to_string()]);
    }

    #[test]
    fn two_d_guard_trims_a_victim_that_wont_fit_on_survivors() {
        // 3 hosts, want to shed 2. Each victim holds 10 GiB reserved; the lone
        // survivor has only 12 GiB free + 4 GiB headroom required. Two victims
        // (20 GiB) don't fit; one (10 GiB) + 4 headroom = 14 ≤ ... no: survivor
        // free after picking 2 victims = just host c's 12 GiB. 20 > 12 → drop
        // one. With 1 victim, survivors = the OTHER loaded host (12 free) + c
        // (12 free) = 24; 10 + 4 ≤ 24 → fits → shed 1.
        let hosts = vec![
            host("v1", 2, 10_000, 12_000),
            host("v2", 2, 10_000, 12_000),
            host("c", 0, 0, 12_000),
        ];
        let d = plan_wave(&hosts, 1, policy(ScaleDownMode::Aggressive, 2, 0, 4_000));
        assert_eq!(d.victims.len(), 1, "2D guard trims the second victim");
        assert!(d.note.contains("2D-guard-trimmed"));
    }

    #[test]
    fn two_d_guard_blocks_all_when_no_survivor_room() {
        // Single loaded host, desired 0. Draining it leaves NO survivor to take
        // its sessions → shed nothing (and don't strand the session).
        let hosts = vec![host("only", 3, 10_000, 0)];
        let d = plan_wave(&hosts, 0, policy(ScaleDownMode::Aggressive, 4, 0, 0));
        assert!(d.victims.is_empty());
        assert!(d.note.contains("2D guard"));
    }

    #[test]
    fn cpu_dimension_can_block_a_victim() {
        // The least-loaded host (picked first) holds 8 reserved vCPU; RAM fits
        // easily on the survivor, but the survivor has only 4 free vCPU < 8 →
        // the CPU dim of the 2D guard trims it → shed nothing.
        let mut v = host("v", 0, 1_000, 1_000); // least loaded → chosen victim
        v.reserved_vcpus = 8;
        v.free_vcpus = 0;
        let mut s = host("s", 2, 5_000, 60_000); // survivor: lots of free RAM…
        s.free_vcpus = 4; // …but < 8 free vCPU
        let hosts = vec![v, s];
        let d = plan_wave(&hosts, 1, policy(ScaleDownMode::Aggressive, 4, 0, 0));
        assert!(
            d.victims.is_empty(),
            "CPU budget binds — no survivor can take the victim's vCPU"
        );
    }
}
