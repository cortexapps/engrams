//! Heartbeat loop. Phase 1 keeps this in-process (the coordinator and
//! host run in the same process when started with --mode=all). Phase 3
//! switches the transport to tonic+gRPC; the wire format is in
//! `engram-protocol`.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_core::SandboxId;
use engram_protocol::{Heartbeat, HeartbeatAck, HostCapacityReport};
use parking_lot::Mutex;

use crate::resource::CapacitySnapshot;

/// Trait the coordinator implements. Phase 1 a single-process binary
/// implements this directly; Phase 3 a tonic client wraps it.
#[async_trait::async_trait]
pub trait HeartbeatSink: Send + Sync {
    async fn send(&self, hb: Heartbeat) -> HeartbeatAck;
}

/// ADR 0018 Phase B: tracks the set of sandboxes whose backing
/// `/dev/nbdN` device has gone unhealthy. Populated by the
/// NBD-probe task (a follow-up commit; see
/// `engram-host-agent::disk_daemon`); consumed by `build_heartbeat`
/// to fill `Heartbeat::nbd_unhealthy`.
///
/// Shipped in commit 4 as an explicit-trigger seam per
/// `[explicit_admin_triggers_for_testability]`: tests + admin
/// endpoints call `insert()` to inject "this sandbox's NBD is
/// degraded", exercising the coord-side evac trigger (commit 5)
/// without needing a real wedged kernel device. The probe-driven
/// real population lands in a follow-up; until then the monitor
/// stays empty in production.
#[derive(Default)]
pub struct NbdHealthMonitor {
    unhealthy: Mutex<Vec<SandboxId>>,
}

impl NbdHealthMonitor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Mark a sandbox unhealthy. Idempotent — duplicate inserts are
    /// silently deduped so a flapping probe doesn't produce a
    /// duplicated heartbeat field.
    pub fn insert(&self, sandbox_id: SandboxId) {
        let mut guard = self.unhealthy.lock();
        if !guard.contains(&sandbox_id) {
            guard.push(sandbox_id);
        }
    }

    /// Clear an entry (probe recovered, sandbox destroyed, etc.).
    /// Idempotent.
    pub fn remove(&self, sandbox_id: SandboxId) {
        self.unhealthy.lock().retain(|id| *id != sandbox_id);
    }

    /// Snapshot the current unhealthy set for inclusion in a
    /// heartbeat. Sorted for deterministic test fixtures; the coord
    /// doesn't depend on order.
    pub fn snapshot(&self) -> Vec<SandboxId> {
        let mut out = self.unhealthy.lock().clone();
        out.sort();
        out
    }

    /// Test-only: drop the entire set. Production has no such call;
    /// entries are cleared by `remove` as the per-sandbox probe
    /// recovers.
    #[cfg(test)]
    pub fn clear(&self) {
        self.unhealthy.lock().clear();
    }
}

pub fn build_heartbeat(
    host_id: engram_core::HostId,
    cap: &CapacitySnapshot,
    running_sandboxes: Vec<SandboxId>,
    draining: bool,
    nbd_health: &NbdHealthMonitor,
) -> Heartbeat {
    Heartbeat {
        host_id,
        sent_at: Utc::now(),
        capacity: HostCapacityReport {
            total_mib: cap.total_mib,
            used_mib: cap.used_mib,
            running_sandboxes: cap.running_sandboxes,
        },
        local_snapshots: Vec::new(),
        running_sandboxes,
        draining,
        ready_images: Vec::new(),
        nbd_unhealthy: nbd_health.snapshot(),
    }
}

pub fn default_interval() -> Duration {
    Duration::from_secs(5)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::HostId;

    fn empty_monitor() -> Arc<NbdHealthMonitor> {
        NbdHealthMonitor::new()
    }

    #[test]
    fn build_heartbeat_copies_capacity_fields() {
        let host = HostId::new();
        let cap = CapacitySnapshot {
            total_mib: 65_536,
            used_mib: 12_288,
            running_sandboxes: 3,
        };
        let hb = build_heartbeat(host, &cap, Vec::new(), false, &empty_monitor());

        assert_eq!(hb.host_id, host);
        assert_eq!(hb.capacity.total_mib, 65_536);
        assert_eq!(hb.capacity.used_mib, 12_288);
        assert_eq!(hb.capacity.running_sandboxes, 3);
        assert!(!hb.draining);
        // Phase 1: the local snapshot field isn't populated by the
        // helper. It gets filled by the host agent before send.
        assert!(hb.local_snapshots.is_empty());
        assert!(hb.running_sandboxes.is_empty());
    }

    #[test]
    fn build_heartbeat_propagates_drain_flag() {
        let cap = CapacitySnapshot::default();
        let hb = build_heartbeat(HostId::new(), &cap, Vec::new(), true, &empty_monitor());
        assert!(hb.draining);
    }

    #[test]
    fn build_heartbeat_preserves_running_sandboxes() {
        // ADR 0009 §2: this field is the input to the coord's reconcile
        // pass. The builder must propagate it through without mutation.
        let sandboxes = vec![SandboxId::new(), SandboxId::new(), SandboxId::new()];
        let hb = build_heartbeat(
            HostId::new(),
            &CapacitySnapshot::default(),
            sandboxes.clone(),
            false,
            &empty_monitor(),
        );
        assert_eq!(hb.running_sandboxes, sandboxes);
    }

    #[test]
    fn sent_at_is_recent() {
        // We don't pin sent_at, but it must be roughly "now" — verifying
        // catches a bug where someone replaces Utc::now() with epoch 0.
        let before = chrono::Utc::now();
        let hb = build_heartbeat(
            HostId::new(),
            &CapacitySnapshot::default(),
            Vec::new(),
            false,
            &empty_monitor(),
        );
        let after = chrono::Utc::now();
        assert!(hb.sent_at >= before);
        assert!(hb.sent_at <= after);
    }

    #[test]
    fn build_heartbeat_propagates_nbd_unhealthy_set() {
        // The monitor is the load-bearing seam: callers (admin
        // endpoint, future probe task, tests) inject sandbox ids
        // here and they show up in the next heartbeat.
        let monitor = NbdHealthMonitor::new();
        let a = SandboxId::new();
        let b = SandboxId::new();
        monitor.insert(a);
        monitor.insert(b);
        // Idempotent: duplicate inserts dedupe.
        monitor.insert(a);
        let hb = build_heartbeat(
            HostId::new(),
            &CapacitySnapshot::default(),
            Vec::new(),
            false,
            &monitor,
        );
        assert_eq!(hb.nbd_unhealthy.len(), 2);
        assert!(hb.nbd_unhealthy.contains(&a));
        assert!(hb.nbd_unhealthy.contains(&b));
    }

    #[test]
    fn monitor_remove_clears_entry() {
        let monitor = NbdHealthMonitor::new();
        let a = SandboxId::new();
        monitor.insert(a);
        monitor.remove(a);
        assert!(monitor.snapshot().is_empty());
        // Remove on absent id is a no-op.
        monitor.remove(SandboxId::new());
        assert!(monitor.snapshot().is_empty());
    }
}
