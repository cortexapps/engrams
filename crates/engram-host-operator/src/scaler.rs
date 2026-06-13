//! ADR 0044 K4: node-pool autoscaling — the operator's *policy* half.
//!
//! The operator reads the coordinator's fleet demand and computes a desired
//! host node-pool size ([`desired_hosts`]); a [`NodePoolScaler`]
//! (`engram_core::traits::cloud`) actuates it. The actuator is per-cloud and
//! lives in its own crate (`engram_cloud_gcp::gke` does GKE); [`NoopScaler`]
//! here is the default that only *logs* the decision, so you can observe what
//! the autoscaler would do without an actuator wired.

use engram_core::traits::cloud::NodePoolScaler;
use engram_core::BackendError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// ADR 0045 Phase E: how aggressively the operator may shrink the host pool.
///
/// Scale-*down* is fundamentally different from scale-up: a host is a whole
/// node, and `NodePoolScaler::set_size` is count-only (it can't pick *which*
/// node the cloud removes), so a blind shrink could delete a node with live
/// microVMs. Safe scale-down therefore drains a *specific* least-loaded node
/// first and then removes *that* node — which needs node-specific cloud
/// removal (a follow-up actuator). Until that lands the operator computes +
/// **logs** the decision but does not actuate, mirroring how K4 scale-up
/// shipped behind the logging `NoopScaler`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ScaleDownMode {
    /// Never shrink (the K4 default — scale-up only).
    #[default]
    Off,
    /// Only shed a host that is fully idle (0 running sandboxes) — no session
    /// is ever disrupted. The conservative first mode.
    IdleOnly,
    /// Shed the least-loaded host even if it carries sessions (they evacuate
    /// to peers). Only sensible once ADR 0045 Phase C makes the move a live
    /// ~10ms teleport; until then it costs a real per-session pause.
    Aggressive,
}

impl ScaleDownMode {
    pub(crate) fn enabled(self) -> bool {
        !matches!(self, ScaleDownMode::Off)
    }
}

/// Logs the decision without touching any cloud — the operator's default +
/// dev/test fallback. Real actuators (e.g. `engram_cloud_gcp::gke`) implement
/// [`NodePoolScaler`] in their own crates and are selected in `main`.
pub struct NoopScaler;

#[async_trait::async_trait]
impl NodePoolScaler for NoopScaler {
    async fn set_size(&self, node_pool: &str, desired: u32) -> Result<(), BackendError> {
        tracing::info!(
            node_pool,
            desired,
            "noop scaler: would set host node-pool size (no cloud actuator configured)"
        );
        Ok(())
    }

    async fn remove_node(&self, node_pool: &str, node_name: &str) -> Result<(), BackendError> {
        tracing::info!(
            node_pool,
            node_name,
            "noop scaler: would remove node (no cloud actuator configured)"
        );
        Ok(())
    }
}

/// Fleet-demand snapshot from `GET /api/admin/fleet/demand`.
///
/// ADR 0048 added the CPU budget dimension + the queue's aggregate demand.
/// Every field the coordinator gained is `#[serde(default)]` so the operator
/// tolerates polling a pre-0048 coordinator mid-rollout (the new fields read
/// as 0 → no CPU pressure, no queue → identical to the K4 RAM-only behavior).
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct FleetDemand {
    pub schedulable_hosts: u32,
    pub free_mib: u64,
    pub total_mib: u64,
    /// ADR 0048: the CPU budget dimension, in overcommit-vcpu units
    /// (Σ `host_cpu_budget` = Σ `cores × overcommit` over schedulable hosts,
    /// and the free remainder). 0 on hosts that haven't reported a core count.
    #[serde(default)]
    pub free_vcpus: u64,
    #[serde(default)]
    pub total_vcpus: u64,
    /// ADR 0048: the queue's aggregate demand — sessions waiting on capacity,
    /// and the RAM/CPU they'll consume. Scale-up must absorb this ON TOP of
    /// the headroom target; ANY queued session hard-blocks scale-down.
    #[serde(default)]
    pub queued_sessions: u64,
    #[serde(default)]
    pub queued_mib: u64,
    #[serde(default)]
    pub queued_vcpus: u64,
}

/// Autoscaling policy inputs (from the HostFleet CR's `autoscaling`).
#[derive(Clone, Copy, Debug)]
pub struct AutoscalePolicy {
    pub min_hosts: u32,
    pub max_hosts: u32,
    /// Keep at least this much free guest-RAM (MiB) across the fleet.
    pub target_free_mib: u64,
    /// ADR 0045 Phase E: whether (and how aggressively) the pool may shrink.
    pub scale_down: ScaleDownMode,
}

/// Pure scaling policy (ADR 0044 K4 + ADR 0045 Phase E + ADR 0048), unit-tested.
/// Result is clamped to `[min_hosts, max_hosts]`.
///
/// **Scale-up is 2D and queue-aware**, covered in ONE jump (so a 1000-session
/// burst is a single `setSize`, not a slow ramp). The required host count is
/// the **max** of the RAM-deficit and CPU-deficit host counts:
/// - RAM deficit = `queued_mib + max(0, target_free_mib − free_mib)` — absorb
///   the queue AND restore the headroom target after placing it.
/// - CPU deficit = `max(0, queued_vcpus − free_vcpus)` — just enough overcommit
///   budget to place the queue (there's no CPU headroom knob; RAM is the hard
///   constraint, CPU a packing budget).
///
/// Each deficit ÷ its average per-host capacity (`div_ceil`) → hosts; the max
/// of the two is added to `current`.
///
/// **Scale-down** (gated by `policy.scale_down`) is **hard-blocked while ANY
/// session is queued** — a non-empty queue means the fleet is capacity-starved,
/// so shrinking is never right. When the queue is empty it returns the **full
/// cost-optimal target** (shed every host whose removal still leaves
/// `free_mib ≥ target_free_mib`); the wave executor bounds the actual shed per
/// reconcile (`maxShedPerWave`) and the reconcile loop gates entry on
/// hysteresis. With `scale_down = Off` this is pure scale-up (never `< current`).
pub fn desired_hosts(demand: FleetDemand, policy: AutoscalePolicy) -> u32 {
    let max = policy.max_hosts.max(policy.min_hosts);
    let current = demand.schedulable_hosts;

    // Empty fleet: no per-host signal to size against — bootstrap to the floor
    // (set min_hosts ≥ 1 unless you really want scale-to-zero). The next tick,
    // with a live host's capacity signal, sizes up to any queued demand.
    if current == 0 {
        return policy.min_hosts.min(max);
    }

    let per_host_mib = demand.total_mib / current as u64; // avg host RAM capacity
    let per_host_vcpus = demand.total_vcpus / current as u64; // avg host CPU budget

    // ---- Scale-UP: cover BOTH dimensions' deficits in one jump ----
    let ram_deficit = demand.queued_mib + policy.target_free_mib.saturating_sub(demand.free_mib);
    let cpu_deficit = demand.queued_vcpus.saturating_sub(demand.free_vcpus);
    let ram_hosts = if per_host_mib > 0 {
        ram_deficit.div_ceil(per_host_mib)
    } else {
        0
    };
    let cpu_hosts = if per_host_vcpus > 0 {
        cpu_deficit.div_ceil(per_host_vcpus)
    } else {
        0
    };
    let add = ram_hosts.max(cpu_hosts);
    if add > 0 {
        return current
            .saturating_add(add as u32)
            .clamp(policy.min_hosts, max);
    }

    // ---- Scale-DOWN: only with an EMPTY queue + a RAM capacity signal ----
    // A queued session means we're starved — never shrink under back-pressure.
    if policy.scale_down.enabled()
        && demand.queued_sessions == 0
        && per_host_mib > 0
        && demand.free_mib > policy.target_free_mib
    {
        // Cost-optimal: shed every host that still leaves the headroom target.
        //   free − k·per_host ≥ target  ⇒  k ≤ (free − target) / per_host
        let sheddable = (demand.free_mib - policy.target_free_mib) / per_host_mib;
        if sheddable > 0 {
            return current
                .saturating_sub(sheddable as u32)
                .clamp(policy.min_hosts, max);
        }
    }
    current.clamp(policy.min_hosts, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demand(hosts: u32, free: u64, total: u64) -> FleetDemand {
        FleetDemand {
            schedulable_hosts: hosts,
            free_mib: free,
            total_mib: total,
            ..Default::default()
        }
    }
    fn policy(min: u32, max: u32, target: u64) -> AutoscalePolicy {
        AutoscalePolicy {
            min_hosts: min,
            max_hosts: max,
            target_free_mib: target,
            scale_down: ScaleDownMode::Off,
        }
    }
    fn policy_sd(min: u32, max: u32, target: u64, mode: ScaleDownMode) -> AutoscalePolicy {
        AutoscalePolicy {
            scale_down: mode,
            ..policy(min, max, target)
        }
    }

    #[test]
    fn holds_when_headroom_satisfied() {
        // 3 hosts, 30 GiB free, target 16 GiB → already enough, hold at 3.
        assert_eq!(
            desired_hosts(demand(3, 30_000, 96_000), policy(1, 10, 16_000)),
            3
        );
    }

    #[test]
    fn scales_up_to_cover_the_deficit() {
        // 2 hosts, 32 GiB total (16 GiB/host), 2 GiB free, target 16 GiB.
        // deficit 14 GiB / 16 GiB-per-host = ceil(0.875) = 1 → 3 hosts.
        assert_eq!(
            desired_hosts(demand(2, 2_000, 32_000), policy(1, 10, 16_000)),
            3
        );
    }

    #[test]
    fn scales_up_multiple_hosts_for_a_big_deficit() {
        // 16 GiB/host, 0 free, target 48 GiB → need 3 more → 4 hosts.
        assert_eq!(
            desired_hosts(demand(1, 0, 16_000), policy(1, 10, 48_000)),
            4
        );
    }

    #[test]
    fn never_exceeds_max() {
        assert_eq!(
            desired_hosts(demand(2, 0, 32_000), policy(1, 3, 1_000_000)),
            3
        );
    }

    #[test]
    fn scale_down_off_never_shrinks_even_with_spare_capacity() {
        // Lots of free headroom but scale_down Off → hold (K4 behavior).
        assert_eq!(
            desired_hosts(demand(5, 80_000, 80_000), policy(1, 10, 16_000)),
            5
        );
    }

    #[test]
    fn empty_fleet_bootstraps_to_min() {
        assert_eq!(desired_hosts(demand(0, 0, 0), policy(2, 10, 16_000)), 2);
    }

    // ---- ADR 0045 Phase E: scale-down arm ----

    #[test]
    fn sheds_one_host_when_a_full_host_of_slack_remains() {
        // 5 hosts, 16 GiB/host (80 GiB total), 40 GiB free, target 16 GiB.
        // After shedding one host we'd still have 40 − 16 = 24 GiB ≥ 16 → shed.
        // Only one host per call (4, not fewer), even with lots of slack.
        assert_eq!(
            desired_hosts(
                demand(5, 40_000, 80_000),
                policy_sd(1, 10, 16_000, ScaleDownMode::IdleOnly)
            ),
            4
        );
    }

    #[test]
    fn holds_when_shedding_would_break_the_headroom_target() {
        // 3 hosts, 16 GiB/host (48 GiB total), 20 GiB free, target 16 GiB.
        // Headroom is satisfied (20 ≥ 16) but shedding one (−16) leaves 4 GiB
        // < 16 → would break the target, so hold at 3.
        assert_eq!(
            desired_hosts(
                demand(3, 20_000, 48_000),
                policy_sd(1, 10, 16_000, ScaleDownMode::Aggressive)
            ),
            3
        );
    }

    #[test]
    fn scale_down_never_drops_below_floor() {
        // Tons of slack but the floor is the current count → hold.
        assert_eq!(
            desired_hosts(
                demand(2, 64_000, 64_000),
                policy_sd(2, 10, 8_000, ScaleDownMode::Aggressive)
            ),
            2
        );
    }

    #[test]
    fn aggressive_and_idle_only_share_the_same_ram_target() {
        // The mode only changes *which host* the actuation drains; the RAM
        // shed *target* is identical. ADR 0048: this is now the COST-OPTIMAL
        // target, not one-at-a-time — 16 GiB/host, 48 free, target 16:
        // shed k while 48 − 16k ≥ 16 → k ≤ 2 → target 2. The wave executor
        // bounds the actual per-wave shed (maxShedPerWave).
        let d = demand(4, 48_000, 64_000);
        assert_eq!(
            desired_hosts(d, policy_sd(1, 10, 16_000, ScaleDownMode::IdleOnly)),
            2
        );
        assert_eq!(
            desired_hosts(d, policy_sd(1, 10, 16_000, ScaleDownMode::Aggressive)),
            2
        );
    }

    // ---- ADR 0048: 2D + queue-aware scale-up ----

    /// A burst that queues a lot of RAM is covered in ONE jump (not a ramp).
    #[test]
    fn one_shot_burst_covers_the_whole_queue_in_one_jump() {
        // 2 hosts, 16 GiB/host, free 0, target 0, queue wants 160 GiB.
        // ram_deficit = 160_000 + 0 = 160_000 / 16_000 = 10 hosts → 12.
        let d = FleetDemand {
            schedulable_hosts: 2,
            free_mib: 0,
            total_mib: 32_000,
            queued_mib: 160_000,
            queued_sessions: 40,
            ..Default::default()
        };
        assert_eq!(desired_hosts(d, policy(1, 100, 0)), 12);
    }

    /// CPU pressure can bind before RAM — the required hosts is the MAX of
    /// the two dimensions' deficits.
    #[test]
    fn cpu_dimension_can_bind_before_ram() {
        // 2 hosts. RAM: 16 GiB/host, free 32 GiB, target 0, no queued_mib →
        // 0 RAM hosts. CPU: 16 budget-vcpu/host (32 total), free 0, queue
        // wants 64 vcpu → ceil(64/16) = 4 CPU hosts. max(0,4) = 4 → 6.
        let d = FleetDemand {
            schedulable_hosts: 2,
            free_mib: 32_000,
            total_mib: 32_000,
            total_vcpus: 32,
            free_vcpus: 0,
            queued_vcpus: 64,
            queued_sessions: 32,
            ..Default::default()
        };
        assert_eq!(desired_hosts(d, policy(1, 100, 0)), 6);
    }

    /// The headroom deficit and the queued demand COMPOSE on the RAM axis.
    #[test]
    fn ram_headroom_and_queue_deficits_compose() {
        // 2 hosts, 16 GiB/host, free 4 GiB, target 16 GiB, queue wants 16 GiB.
        // ram_deficit = 16_000 (queue) + (16_000 − 4_000) (headroom) = 28_000
        // / 16_000 = ceil(1.75) = 2 → 4.
        let d = FleetDemand {
            schedulable_hosts: 2,
            free_mib: 4_000,
            total_mib: 32_000,
            queued_mib: 16_000,
            queued_sessions: 4,
            ..Default::default()
        };
        assert_eq!(desired_hosts(d, policy(1, 100, 16_000)), 4);
    }

    /// A queue-driven scale-up still clamps to max_hosts.
    #[test]
    fn queue_driven_scale_up_clamps_to_max() {
        let d = FleetDemand {
            schedulable_hosts: 2,
            free_mib: 0,
            total_mib: 32_000,
            queued_mib: 1_600_000,
            queued_sessions: 400,
            ..Default::default()
        };
        assert_eq!(desired_hosts(d, policy(1, 5, 0)), 5);
    }

    /// The `queued_sessions == 0` scale-down gate is DEFENSIVE: a queue with
    /// real RAM/CPU demand already triggers scale-*up* (it can't reach the
    /// scale-down branch). The gate guards the degenerate case — a session
    /// queued with no measurable budget (e.g. blocked purely on
    /// image-readiness, not capacity) — so huge RAM slack still can't shrink
    /// the fleet while anything is waiting.
    #[test]
    fn any_queued_session_blocks_scale_down() {
        // 5 hosts, 80 GiB free (cost-optimal would shed several), but one
        // session is queued with zero measured demand → hold at 5.
        let d = FleetDemand {
            schedulable_hosts: 5,
            free_mib: 80_000,
            total_mib: 80_000,
            queued_sessions: 1,
            queued_mib: 0,
            queued_vcpus: 0,
            ..Default::default()
        };
        assert_eq!(
            desired_hosts(d, policy_sd(1, 10, 16_000, ScaleDownMode::Aggressive)),
            5
        );
    }

    /// A queued session WITH real RAM demand triggers scale-up (it never
    /// reaches the scale-down branch) — the complement to the gate test.
    #[test]
    fn queued_session_with_demand_triggers_scale_up_not_hold() {
        // 5 hosts, 16 GiB/host, 80 GiB free, target 16 GiB, queue wants 2 GiB.
        // ram_deficit = 2_000 + 0 = 2_000 → ceil(2000/16000) = 1 → grow to 6.
        let d = FleetDemand {
            schedulable_hosts: 5,
            free_mib: 80_000,
            total_mib: 80_000,
            queued_sessions: 1,
            queued_mib: 2_000,
            ..Default::default()
        };
        assert_eq!(
            desired_hosts(d, policy_sd(1, 10, 16_000, ScaleDownMode::Aggressive)),
            6
        );
    }

    /// Cost-optimal scale-down sheds every host that keeps the headroom.
    #[test]
    fn scale_down_returns_the_full_cost_optimal_target() {
        // 10 hosts, 16 GiB/host, 100 GiB free, target 16 GiB, empty queue.
        // k while 100 − 16k ≥ 16 → k ≤ 5.25 → 5 → target 5.
        let d = demand(10, 100_000, 160_000);
        assert_eq!(
            desired_hosts(d, policy_sd(1, 20, 16_000, ScaleDownMode::Aggressive)),
            5
        );
    }
}
