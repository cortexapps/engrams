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
    fn enabled(self) -> bool {
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
}

/// Fleet-demand snapshot from `GET /api/admin/fleet/demand`.
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct FleetDemand {
    pub schedulable_hosts: u32,
    pub free_mib: u64,
    pub total_mib: u64,
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

/// Pure scaling policy (ADR 0044 K4 + ADR 0045 Phase E), unit-tested. Result
/// is clamped to `[min_hosts, max_hosts]`.
///
/// **Scale-up** grows the pool until free headroom ≥ `target_free_mib`.
///
/// **Scale-down** (ADR 0045 Phase E, gated by `policy.scale_down`) sheds **at
/// most one host per call**, and only when a *whole host's worth* of slack
/// remains afterward (`free_mib ≥ target_free_mib + per_host_mib`) — so the
/// fleet never dips below its headroom target by shedding. One-at-a-time +
/// the headroom guard keep it conservative; the reconcile loop adds hysteresis
/// (act only after N consecutive agreeing ticks) on top. With `scale_down =
/// Off` this is pure scale-up (the K4 behavior), never returning `< current`.
pub fn desired_hosts(demand: FleetDemand, policy: AutoscalePolicy) -> u32 {
    let max = policy.max_hosts.max(policy.min_hosts);
    let current = demand.schedulable_hosts;

    // Empty fleet: nothing to measure per-host against — bootstrap to the
    // floor (so set min_hosts ≥ 1 unless you really want scale-to-zero).
    if current == 0 {
        return policy.min_hosts.min(max);
    }

    let per_host_mib = demand.total_mib / current as u64; // average host capacity
    if per_host_mib == 0 {
        return current.clamp(policy.min_hosts, max); // no capacity signal — hold
    }

    if demand.free_mib < policy.target_free_mib {
        // Under the headroom target — grow to cover the deficit.
        let deficit = policy.target_free_mib - demand.free_mib;
        let up = current.saturating_add(deficit.div_ceil(per_host_mib) as u32);
        return up.clamp(policy.min_hosts, max);
    }

    // Headroom satisfied. Shed one host iff scale-down is enabled AND a full
    // host of slack would still remain — otherwise hold.
    if policy.scale_down.enabled() && demand.free_mib >= policy.target_free_mib + per_host_mib {
        return current.saturating_sub(1).clamp(policy.min_hosts, max);
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
    fn aggressive_and_idle_only_share_the_same_ram_policy() {
        // The mode only changes *which host* the actuation drains; the RAM
        // shed decision is identical. Both shed here.
        let d = demand(4, 48_000, 64_000); // 16 GiB/host, 48 free, shed leaves 32 ≥ 16
        assert_eq!(
            desired_hosts(d, policy_sd(1, 10, 16_000, ScaleDownMode::IdleOnly)),
            3
        );
        assert_eq!(
            desired_hosts(d, policy_sd(1, 10, 16_000, ScaleDownMode::Aggressive)),
            3
        );
    }
}
