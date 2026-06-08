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
use serde::Deserialize;

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
}

/// Pure scaling policy (ADR 0044 K4), unit-tested.
///
/// **Scale-up only:** grow the pool until free headroom ≥ `target_free_mib`;
/// never shrink. Drain-gated scale-down (removing a host node only after its
/// sessions evacuate) is a deferred follow-up — see the ADR. Result is
/// clamped to `[min_hosts, max_hosts]`.
pub fn desired_hosts(demand: FleetDemand, policy: AutoscalePolicy) -> u32 {
    let max = policy.max_hosts.max(policy.min_hosts);
    let current = demand.schedulable_hosts;

    // Empty fleet: nothing to measure per-host against — bootstrap to the
    // floor (so set min_hosts ≥ 1 unless you really want scale-to-zero).
    if current == 0 {
        return policy.min_hosts.min(max);
    }

    let per_host_mib = demand.total_mib / current as u64; // average host capacity
    let desired = if demand.free_mib >= policy.target_free_mib || per_host_mib == 0 {
        current // headroom satisfied (or no capacity signal) — hold, don't shed
    } else {
        let deficit = policy.target_free_mib - demand.free_mib;
        current.saturating_add(deficit.div_ceil(per_host_mib) as u32)
    };
    desired.clamp(policy.min_hosts, max)
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
    fn never_shrinks_below_current_even_with_spare_capacity() {
        // Lots of free headroom: a scale-down candidate, but v1 holds.
        assert_eq!(
            desired_hosts(demand(5, 80_000, 80_000), policy(1, 10, 16_000)),
            5
        );
    }

    #[test]
    fn empty_fleet_bootstraps_to_min() {
        assert_eq!(desired_hosts(demand(0, 0, 0), policy(2, 10, 16_000)), 2);
    }
}
