//! The SIGTERM shutdown decisions: how long to drain in-flight captures,
//! and the handoff marker a shutting-down host-agent leaves for its
//! successor. On SIGTERM the host-agent stops admitting captures, waits up
//! to the drain budget for the ones in flight, and exits. Session volumes
//! are kernel-owned and outlive the process, so nothing else needs saving.

use std::time::Duration;

/// Default capture-drain budget (seconds) when
/// `ENGRAM_SHUTDOWN_CAPTURE_DRAIN_BUDGET_SECS` is unset, unparseable, or
/// non-positive. A diff capture in flight at SIGTERM has already consumed
/// the KVM dirty bitmap; if the process exits under it, the checkpoint
/// chain is poisoned. 30 s covers a normal diff finalize with room for a
/// slow upload and fits the DaemonSet's `terminationGracePeriodSeconds`.
pub const DEFAULT_CAPTURE_DRAIN_BUDGET_SECS: f64 = 30.0;

/// The capture-drain deadline from a pre-parsed
/// `ENGRAM_SHUTDOWN_CAPTURE_DRAIN_BUDGET_SECS` value. Non-positive or
/// absent gives the default.
pub fn capture_drain_budget(env_value: Option<f64>) -> Duration {
    let secs = env_value
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_CAPTURE_DRAIN_BUDGET_SECS);
    Duration::from_secs_f64(secs)
}

/// ADR 0116 A-D2: the durable note a shutting-down host-agent leaves in
/// its bindings directory for the successor generation — proof this was
/// a PLANNED exit with a declared lease deadline. The successor adopts
/// it (read + delete) and reports it on register; the coordinator uses
/// it for observability only (register renews the lease regardless).
/// Written FIRST in the SIGTERM ladder, before the heartbeat task is
/// aborted, so the belt POST and the durable note both precede any
/// silence the coordinator could observe.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HandoffMarker {
    /// Predecessor wall clock at declaration (unix ms). Same-node clock;
    /// adoption latency derived from it is a histogram input, never a
    /// decision.
    pub declared_at_unix_ms: i64,
    /// TTL the predecessor requested for its handoff window.
    pub ttl_secs: u64,
    /// Sandboxes resident at declaration (diagnostics).
    #[serde(default)]
    pub resident_sandboxes: u32,
}

/// File name of the handoff marker inside the bindings directory.
pub const HANDOFF_MARKER_FILE: &str = "handoff.json";

/// Default handoff TTL the host's own SIGTERM ladder declares (the belt;
/// the operator's authoritative declaration is sized to its roll gates
/// instead). 600 s covers a kubelet restart + image pull on a slow node
/// (the 2026-08-12 replacement took 5.5 min). Env
/// `ENGRAM_SHUTDOWN_HANDOFF_TTL_SECS` overrides.
pub fn shutdown_handoff_ttl_secs(env_val: Option<&str>) -> u64 {
    env_val
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_drain_budget_defaults_on_absent_or_non_positive() {
        assert_eq!(
            capture_drain_budget(None),
            Duration::from_secs_f64(DEFAULT_CAPTURE_DRAIN_BUDGET_SECS)
        );
        assert_eq!(
            capture_drain_budget(Some(0.0)),
            Duration::from_secs_f64(DEFAULT_CAPTURE_DRAIN_BUDGET_SECS)
        );
        assert_eq!(
            capture_drain_budget(Some(-1.0)),
            Duration::from_secs_f64(DEFAULT_CAPTURE_DRAIN_BUDGET_SECS)
        );
        assert_eq!(
            capture_drain_budget(Some(12.5)),
            Duration::from_secs_f64(12.5)
        );
    }

    #[test]
    fn shutdown_handoff_ttl_parses_with_default() {
        assert_eq!(shutdown_handoff_ttl_secs(None), 600);
        assert_eq!(shutdown_handoff_ttl_secs(Some("120")), 120);
        assert_eq!(shutdown_handoff_ttl_secs(Some("0")), 600);
        assert_eq!(shutdown_handoff_ttl_secs(Some("junk")), 600);
    }
}
