//! `/dev/nbdN` slot allocator.
//!
//! The Linux kernel exposes a fixed number of NBD devices
//! (`/dev/nbd0`..`/dev/nbdN`) sized by the `nbds_max` module
//! parameter at `modprobe` time. The host-agent treats them as a
//! pool: each FC sandbox grabs a slot at create time, holds it for
//! the VM's lifetime, and returns it on destroy.
//!
//! Allocator semantics:
//!
//! - **Free list** is the source of truth. On `acquire()`, pop one
//!   path. On `release(path)`, push it back. No reference counting
//!   — each path is held by exactly one sandbox at a time.
//! - **Async-friendly**. `acquire()` is async because under
//!   pressure (more sandboxes than slots) callers need to wait
//!   rather than fail. Wrapping a `tokio::sync::Mutex` + a
//!   `Notify` makes the wait wake-up explicit.
//! - **No automatic device-file probing.** Operators populate the
//!   pool explicitly with the paths they want available — that
//!   way an accidental `/dev/nbd17` (outside the configured
//!   `nbds_max`) can't sneak in.
//!
//! Production wiring: the host-agent's startup reads
//! `ENGRAM_NBD_DEVICES=/dev/nbd0,/dev/nbd1,...` (env var; defaults
//! to empty) and instantiates one `NbdSlotAllocator` from that
//! list. The Packer manifest loads the `nbd` kernel module with
//! `nbds_max` matching.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};

/// ADR 0017 Phase A: probe the kernel-side binding state of a
/// `/dev/nbdN` device. Returns `true` iff `/sys/block/nbdN/pid`
/// exists and has non-empty contents — the kernel's signal that
/// the device is currently bound to an NBD daemon thread.
///
/// Used by [`NbdSlotAllocator::acquire`] to skip slots whose
/// kernel-side cleanup is still in flight (the destroy path's
/// detached `kernel_thread.join()` hasn't completed yet) AND
/// slots whose bound PID is dead but the kernel hasn't released
/// (the Phase B startup-cleanup target). Both cases produce a
/// non-empty pid file; the probe treats them identically: skip
/// for now, re-probe later.
///
/// On non-Linux platforms (macOS dev), `/sys/block` doesn't
/// exist; the probe returns `false` (not busy) so the in-process
/// slot pool used by tests on Mac stays functional.
fn nbd_kernel_busy(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        let pid_path = format!("/sys/block/{name}/pid");
        match std::fs::read_to_string(&pid_path) {
            Ok(s) => !s.trim().is_empty(),
            // ENOENT: device has never been bound (or the kernel
            // released the binding). Either way, not busy from our
            // perspective.
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        false
    }
}

/// Lease handle for one `/dev/nbdN` slot. Auto-returns the slot to
/// the allocator on `Drop` — sandboxes hold one of these for the
/// lifetime of their NBD daemon and the lease's drop is what
/// releases the slot back into the pool.
pub struct NbdSlot {
    path: PathBuf,
    allocator: Arc<NbdSlotAllocator>,
}

impl NbdSlot {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for NbdSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NbdSlot").field("path", &self.path).finish()
    }
}

impl Drop for NbdSlot {
    fn drop(&mut self) {
        let path = std::mem::take(&mut self.path);
        let allocator = self.allocator.clone();
        // Spawn a small blocking-task-free release: locking is
        // synchronous via try_lock_owned would be cleaner, but
        // tokio's Mutex needs async. Spawn so the Drop doesn't
        // block on a busy lock. Fire-and-forget is safe because
        // the path is unique per slot and release is idempotent
        // at the allocator level.
        tokio::spawn(async move {
            allocator.release(path).await;
        });
    }
}

/// Pool of `/dev/nbdN` device paths. Cheap to clone via `Arc`.
#[derive(Debug)]
pub struct NbdSlotAllocator {
    free: Mutex<VecDeque<PathBuf>>,
    notify: Notify,
    capacity: usize,
}

impl NbdSlotAllocator {
    /// Build from a list of device paths. Each path is checked for
    /// uniqueness; duplicates are rejected loud so a misconfigured
    /// operator can't accidentally double-allocate the same
    /// device.
    pub fn from_paths(paths: Vec<PathBuf>) -> Result<Arc<Self>, String> {
        let mut seen = std::collections::HashSet::new();
        for p in &paths {
            if !seen.insert(p.clone()) {
                return Err(format!(
                    "duplicate NBD device path in pool: {}",
                    p.display()
                ));
            }
        }
        let capacity = paths.len();
        Ok(Arc::new(Self {
            free: Mutex::new(paths.into()),
            notify: Notify::new(),
            capacity,
        }))
    }

    /// Total slot count. Useful for telemetry / capacity reports.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Wait for + claim a free slot. Returns immediately when the
    /// pool has a free path; otherwise sleeps until a sibling
    /// sandbox releases one.
    ///
    /// ADR 0017 Phase A: before handing out a path, probe its
    /// kernel-side `/sys/block/nbdN/pid` to confirm the device is
    /// actually free. The destroy path detaches the
    /// kernel-thread join into a `std::thread::spawn` so the
    /// destroy RPC returns immediately, but the kernel side may
    /// still be cleaning up when the slot returns to the pool.
    /// Without this probe, a fast re-acquire would hand out a
    /// path whose `NBD_SET_SOCK` fails with EBUSY.
    ///
    /// The probe is best-effort: a missing `/sys/block/nbdN/pid`
    /// is treated as "not busy" (matches kernel semantics when
    /// the device has never been bound). On macOS the cfg-gated
    /// no-op path is used; the slot pool is target-agnostic
    /// but the probe is Linux-only.
    pub async fn acquire(self: &Arc<Self>) -> NbdSlot {
        loop {
            {
                let mut free = self.free.lock().await;
                let initial_len = free.len();
                let mut probed = 0usize;
                // Rotate-and-probe: pop, check kernel-busy, push to
                // back if busy and continue. Bounded by the deque
                // length so we don't spin forever when every slot
                // is kernel-busy.
                while let Some(path) = free.pop_front() {
                    if !nbd_kernel_busy(&path) {
                        return NbdSlot {
                            path,
                            allocator: self.clone(),
                        };
                    }
                    tracing::debug!(
                        device = %path.display(),
                        "NBD slot kernel-busy (/sys/block/.../pid populated); skipping",
                    );
                    free.push_back(path);
                    probed += 1;
                    if probed >= initial_len {
                        // Cycled through every slot in the deque,
                        // all busy. Fall through to the wait below.
                        break;
                    }
                }
            }
            // Lock dropped before await: standard tokio Notify
            // pattern. `notified()` registers interest BEFORE the
            // re-check, so a release-then-wait race can't miss a
            // wakeup.
            //
            // Wake sources: another sandbox's NbdSlot::Drop fires
            // a release; OR a slot's kernel-busy state clears and
            // a separate caller re-probes it. The second source is
            // best-effort — kernel state isn't observable as an
            // event, so we just rely on the next caller's probe
            // to pick up the cleared slot. To avoid permanent
            // wait when only that path opens, periodic re-probe
            // happens via tokio::time::sleep + retry — short
            // timeout so a freshly-cleared kernel-stuck slot gets
            // picked up within seconds, not minutes.
            tokio::select! {
                _ = self.notify.notified() => {},
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {},
            }
        }
    }

    /// Claim a SPECIFIC device out of the pool — the survivor-
    /// rehydrate path, where the kernel already serves the device
    /// under a surviving FC and the new host-agent generation must
    /// take ownership of exactly that slot (then RECONFIGURE it)
    /// rather than acquire a fresh one. Deliberately skips the
    /// kernel-busy probe: a survivor's device is busy BY DESIGN.
    ///
    /// `None` if the path isn't in the free list (not part of this
    /// pool, or already held by another lease).
    pub async fn claim(self: &Arc<Self>, path: &Path) -> Option<NbdSlot> {
        let mut free = self.free.lock().await;
        let pos = free.iter().position(|p| p == path)?;
        let path = free.remove(pos)?;
        Some(NbdSlot {
            path,
            allocator: self.clone(),
        })
    }

    /// Snapshot of the currently-free device paths. Used by the
    /// post-rehydrate startup recovery to scope its stale-binding
    /// sweep to slots NOT claimed by surviving sandboxes.
    pub async fn free_paths(&self) -> Vec<PathBuf> {
        self.free.lock().await.iter().cloned().collect()
    }

    /// Return a path to the pool. Wakes one waiter (if any).
    /// `Drop` on `NbdSlot` calls this; direct callers shouldn't
    /// need to.
    async fn release(&self, path: PathBuf) {
        let mut free = self.free.lock().await;
        free.push_back(path);
        self.notify.notify_one();
    }

    /// Current count of free slots. Cheap snapshot for heartbeat
    /// telemetry; not part of the acquisition critical path.
    pub async fn free_count(&self) -> usize {
        self.free.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn paths(n: usize) -> Vec<PathBuf> {
        // ADR 0017 Phase A: `nbd_kernel_busy` probes `/sys/block/nbdN/pid`
        // on Linux. Using real `/dev/nbdN` device names would make these
        // tests host-dependent — a stuck NBD binding on dev-vm hangs
        // `acquire()` indefinitely. Sentinel paths beneath `/dev/` that
        // don't shadow a `/sys/block/` entry probe as "not busy" (the
        // ENOENT branch in `nbd_kernel_busy`).
        (0..n)
            .map(|i| PathBuf::from(format!("/dev/test-fake-nbd-{i}")))
            .collect()
    }

    #[tokio::test]
    async fn acquires_each_slot_until_pool_empties() {
        let pool = NbdSlotAllocator::from_paths(paths(3)).unwrap();
        let s0 = pool.acquire().await;
        let s1 = pool.acquire().await;
        let s2 = pool.acquire().await;
        assert_eq!(pool.free_count().await, 0);

        // All three are distinct device paths.
        let mut seen = std::collections::HashSet::new();
        for s in [&s0, &s1, &s2] {
            assert!(seen.insert(s.path().to_path_buf()));
        }
    }

    #[tokio::test]
    async fn release_wakes_pending_acquire() {
        let pool = NbdSlotAllocator::from_paths(paths(1)).unwrap();
        let first = pool.acquire().await;
        // Second acquire should block; spawn it and assert it
        // hasn't completed within a tight bound.
        let pool2 = pool.clone();
        let task = tokio::spawn(async move { pool2.acquire().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !task.is_finished(),
            "second acquire must block while pool is full"
        );

        // Drop first → release → second completes.
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("second acquire didn't wake within 1s")
            .expect("acquire task panicked");
        assert!(matches!(
            second.path().to_str(),
            Some("/dev/test-fake-nbd-0")
        ));
    }

    #[tokio::test]
    async fn drop_returns_path_to_pool() {
        let pool = NbdSlotAllocator::from_paths(paths(1)).unwrap();
        {
            let _slot = pool.acquire().await;
            assert_eq!(pool.free_count().await, 0);
        }
        // Drop is async via tokio::spawn — give it a moment to run.
        for _ in 0..20 {
            if pool.free_count().await == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("slot was not released back to the pool after drop");
    }

    #[test]
    fn rejects_duplicate_paths_in_pool() {
        let dup = vec![PathBuf::from("/dev/nbd0"), PathBuf::from("/dev/nbd0")];
        let err = NbdSlotAllocator::from_paths(dup).unwrap_err();
        assert!(err.contains("/dev/nbd0"));
    }

    #[tokio::test]
    async fn capacity_reports_total_slot_count() {
        let pool = NbdSlotAllocator::from_paths(paths(7)).unwrap();
        assert_eq!(pool.capacity(), 7);
        assert_eq!(pool.free_count().await, 7);
        let _s = pool.acquire().await;
        assert_eq!(pool.free_count().await, 6);
        // capacity stays constant even after acquisition.
        assert_eq!(pool.capacity(), 7);
    }
}
