//! Per-VM resource accounting and limits enforcement.
//!
//! The SandboxBackend is responsible for *applying* limits at VM start
//! (cgroups, Firecracker config). This module just tracks aggregate
//! capacity for heartbeat reporting.

use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct CapacitySnapshot {
    pub total_mib: u64,
    pub used_mib: u64,
    pub running_sandboxes: u32,
}

#[derive(Clone, Default)]
pub struct ResourceGovernor {
    inner: Arc<Mutex<CapacitySnapshot>>,
}

impl ResourceGovernor {
    pub fn new(total_mib: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(CapacitySnapshot {
                total_mib,
                ..Default::default()
            })),
        }
    }

    pub fn record_start(&self, mib: u64) {
        let mut g = self.inner.lock();
        g.used_mib = g.used_mib.saturating_add(mib);
        g.running_sandboxes += 1;
    }

    pub fn record_stop(&self, mib: u64) {
        let mut g = self.inner.lock();
        g.used_mib = g.used_mib.saturating_sub(mib);
        g.running_sandboxes = g.running_sandboxes.saturating_sub(1);
    }

    pub fn snapshot(&self) -> CapacitySnapshot {
        self.inner.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_start_and_stop_roundtrip() {
        let g = ResourceGovernor::new(64_000);
        g.record_start(8_192);
        g.record_start(4_096);
        let mid = g.snapshot();
        assert_eq!(mid.total_mib, 64_000);
        assert_eq!(mid.used_mib, 12_288);
        assert_eq!(mid.running_sandboxes, 2);

        g.record_stop(8_192);
        let after = g.snapshot();
        assert_eq!(after.used_mib, 4_096);
        assert_eq!(after.running_sandboxes, 1);
    }

    #[test]
    fn stop_without_matching_start_saturates_at_zero() {
        // A double-stop or a stop that races a forced-eviction must not
        // wrap around to u64::MAX — that would poison capacity reporting.
        let g = ResourceGovernor::new(1_000);
        g.record_stop(100);
        g.record_stop(100);
        let s = g.snapshot();
        assert_eq!(s.used_mib, 0);
        assert_eq!(s.running_sandboxes, 0);
    }

    #[test]
    fn used_mib_does_not_overflow_total() {
        // Caller may oversubscribe (e.g. due to spec drift); the
        // governor must still saturate-add cleanly without panicking.
        let g = ResourceGovernor::new(100);
        g.record_start(u64::MAX - 10);
        g.record_start(100);
        let s = g.snapshot();
        assert_eq!(s.used_mib, u64::MAX);
    }

    #[test]
    fn snapshot_reflects_current_state_each_call() {
        let g = ResourceGovernor::new(2_000);
        assert_eq!(g.snapshot().used_mib, 0);
        g.record_start(500);
        assert_eq!(g.snapshot().used_mib, 500);
        g.record_start(500);
        assert_eq!(g.snapshot().used_mib, 1_000);
    }
}
