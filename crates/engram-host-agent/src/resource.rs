//! Per-VM resource accounting and limits enforcement.
//!
//! The SandboxBackend is responsible for *applying* limits at VM start
//! (cgroups, Firecracker config). This module just tracks aggregate
//! capacity for heartbeat reporting.

use parking_lot::Mutex;
use std::sync::Arc;

/// Read total system memory in MiB. Used to seed
/// `ResourceGovernor::new(...)` at host-agent startup so capacity
/// heartbeats report the actual machine size — without this, the
/// coord scheduler sees `total_mib=0` on every host and rejects every
/// session whose `vm_spec.memory.max_mib` is non-zero (every session
/// in practice), and the idle-evictor's `host_has_memory_headroom`
/// fails closed so rung-2 park never fires (ADR 0096 — macOS/VZ hosts
/// reported 0 until this grew a Darwin arm). Returns 0 on failure —
/// the caller logs and continues; sessions just won't be scheduled to
/// the host, which is the safe default.
#[cfg(target_os = "linux")]
pub fn read_total_memory_mib() -> u64 {
    let s = match std::fs::read_to_string("/proc/meminfo") {
        Ok(s) => s,
        Err(_) => return 0,
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            // Format: "MemTotal:       32910156 kB"
            if let Some(kb) = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u64>().ok())
            {
                return kb / 1024;
            }
        }
    }
    0
}

/// Darwin arm: `sysctl hw.memsize` (u64 bytes). No /proc on macOS.
#[cfg(target_os = "macos")]
pub fn read_total_memory_mib() -> u64 {
    let mut bytes: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    let name = c"hw.memsize";
    // SAFETY: sysctlbyname writes at most `len` bytes into `bytes`,
    // which is exactly sized for the u64 hw.memsize returns.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut bytes as *mut u64 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return 0;
    }
    bytes / (1024 * 1024)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn read_total_memory_mib() -> u64 {
    0
}

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

    /// ADR 0096: Linux and macOS must both report real capacity — a 0
    /// here starves the scheduler fit-check and the rung-2 park
    /// headroom gate. (Other platforms keep the 0 fallback.)
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn read_total_memory_is_plausible() {
        let mib = read_total_memory_mib();
        assert!(
            mib > 1024,
            "host reports {mib} MiB — capacity probe broken on this platform"
        );
    }

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
