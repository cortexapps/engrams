//! The hydration of a sandbox's cloned disks (ADR 0127 Decisions 5-7):
//! one task per sandbox keeps each source serving and cuts each disk
//! over to its destination once it is copied. The task belongs to the
//! sandbox: `destroy` stops it, and after a host-agent restart it starts
//! again for a live sandbox with a disk left, from the bitmap the kernel
//! kept. A source failure never ends the sandbox: the task logs and tries
//! again, later each time the same disk fails again.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use dashmap::DashMap;
use engram_core::types::snapshot::DiskRole;
use engram_core::SandboxId;
use engram_host_core::session_volumes::{Cutover, Hydrating, Waiting};
use engram_host_core::SessionVolumes;

use crate::pooled_backend::{capture_lock_in, AbortTaskOnDrop, CaptureLocks};

/// How often a task looks at its disks unless the host sets it.
pub(crate) const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);

/// The longest wait before a disk whose step failed is tried again.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// The hydration task of each sandbox that has one, with the generation
/// that armed it.
pub(crate) struct Hydrations {
    tasks: DashMap<SandboxId, (u64, AbortTaskOnDrop)>,
    generation: AtomicU64,
    interval: Duration,
}

impl Hydrations {
    pub(crate) fn new(interval: Duration) -> Arc<Self> {
        Arc::new(Self {
            tasks: DashMap::new(),
            generation: AtomicU64::new(0),
            interval,
        })
    }

    /// Start the sandbox's task, unless it has one. The task ends when no
    /// disk is left to cut over. It takes the sandbox's capture lock for
    /// each cutover, which serializes the cutover with the checkpoints.
    pub(crate) fn arm(
        self: &Arc<Self>,
        id: SandboxId,
        volumes: Arc<SessionVolumes>,
        capture_locks: Arc<CaptureLocks>,
    ) {
        let dashmap::Entry::Vacant(slot) = self.tasks.entry(id) else {
            return;
        };
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        let owner: Weak<Self> = Arc::downgrade(self);
        let interval = self.interval;
        let task = tokio::spawn(async move {
            hydrate(&volumes, &capture_locks, id, interval).await;
            // An abort takes effect only at an await, so a disarmed task can
            // get here; it then removes nothing that a later `arm` put in.
            if let Some(owner) = owner.upgrade() {
                if owner
                    .tasks
                    .remove_if(&id, |_, (armed, _)| *armed == generation)
                    .is_some()
                {
                    owner.publish_gauge();
                }
            }
        });
        slot.insert((generation, AbortTaskOnDrop(task)));
        self.publish_gauge();
    }

    /// Stop the sandbox's task.
    pub(crate) fn disarm(&self, id: SandboxId) {
        if self.tasks.remove(&id).is_some() {
            self.publish_gauge();
        }
    }

    /// The sandboxes with a task.
    pub(crate) fn sandboxes(&self) -> Vec<SandboxId> {
        let mut ids: Vec<SandboxId> = self.tasks.iter().map(|e| *e.key()).collect();
        ids.sort();
        ids
    }

    fn publish_gauge(&self) {
        metrics::gauge!(crate::metrics::DISK_HYDRATING_SANDBOXES).set(self.tasks.len() as f64);
    }
}

/// When each disk whose step failed is tried again, in ticks.
#[derive(Default)]
struct Backoff {
    roles: HashMap<DiskRole, (u32, u64)>,
}

impl Backoff {
    /// Whether `role` is due at `tick`.
    fn due(&self, role: DiskRole, tick: u64) -> bool {
        self.roles.get(&role).is_none_or(|&(_, next)| tick >= next)
    }

    /// Record the outcome of `role`'s step at `tick`. Each failure in a row
    /// doubles the wait, up to `max_ticks`; a success clears it.
    fn record(&mut self, role: DiskRole, tick: u64, failed: bool, max_ticks: u64) {
        if !failed {
            self.roles.remove(&role);
            return;
        }
        let failures = self.roles.get(&role).map_or(0, |&(f, _)| f) + 1;
        let wait = 1u64
            .checked_shl(failures - 1)
            .unwrap_or(u64::MAX)
            .min(max_ticks.max(1));
        self.roles.insert(role, (failures, tick + wait));
    }
}

/// Tick until no disk of the sandbox is left to cut over. A failure is
/// logged when it differs from the failures of the tick before.
async fn hydrate(
    volumes: &SessionVolumes,
    capture_locks: &CaptureLocks,
    id: SandboxId,
    interval: Duration,
) {
    let armed = crate::time_source::metrics_now();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let max_ticks = (MAX_BACKOFF.as_millis() / interval.as_millis().max(1)) as u64;
    let mut backoff = Backoff::default();
    let mut reported: Vec<String> = Vec::new();
    for tick in 0u64.. {
        ticker.tick().await;
        let pass = pass(volumes, capture_locks, id, tick, max_ticks, &mut backoff).await;
        if pass.errors != reported {
            for error in &pass.errors {
                tracing::warn!(sandbox_id = %id, %error, "disk hydration failed; retrying");
            }
            reported = pass.errors;
        }
        if !pass.left {
            break;
        }
    }
    metrics::histogram!(crate::metrics::DISK_HYDRATION_SECONDS)
        .record(armed.elapsed().as_secs_f64());
    tracing::info!(sandbox_id = %id, "disk hydration finished");
}

struct Pass {
    /// A disk is still to cut over, or its step failed.
    left: bool,
    errors: Vec<String>,
}

async fn pass(
    volumes: &SessionVolumes,
    capture_locks: &CaptureLocks,
    id: SandboxId,
    tick: u64,
    max_ticks: u64,
    backoff: &mut Backoff,
) -> Pass {
    let roles = match volumes.roles(id).await {
        Ok(roles) => roles,
        Err(e) => {
            return Pass {
                left: true,
                errors: vec![format!("list the disks: {e}")],
            }
        }
    };
    let mut pass = Pass {
        left: false,
        errors: Vec::new(),
    };
    for role in roles {
        if !backoff.due(role, tick) {
            pass.left = true;
            continue;
        }
        let result = step(volumes, capture_locks, id, role).await;
        backoff.record(role, tick, result.is_err(), max_ticks);
        match result {
            Ok(done) => pass.left |= !done,
            Err(e) => {
                pass.left = true;
                pass.errors.push(format!("{role} disk: {e}"));
            }
        }
    }
    pass
}

/// Move one disk on; `true` when nothing of it is left to cut over.
/// `hydration` recovers a dead source before the capture lock is taken,
/// and the source stops after the lock is released: neither can hold up
/// a checkpoint for a process start or stop.
async fn step(
    volumes: &SessionVolumes,
    capture_locks: &CaptureLocks,
    id: SandboxId,
    role: DiskRole,
) -> io::Result<bool> {
    match volumes.hydration(id, role).await? {
        None => Ok(true),
        Some(Hydrating::Copying(_)) => Ok(false),
        Some(Hydrating::Ready) => {
            let cut = {
                let lock = capture_lock_in(capture_locks, id);
                let _held = lock.lock().await;
                volumes.cut_over(id, role).await
            };
            let outcome = match &cut {
                Ok(Cutover::Done) => "done",
                Ok(Cutover::Waiting(Waiting::Checkpoint)) => "checkpoint_in_flight",
                Ok(Cutover::Waiting(Waiting::Copying(_))) => "copying",
                Err(_) => "error",
            };
            metrics::counter!(crate::metrics::DISK_CUTOVERS_TOTAL, "outcome" => outcome)
                .increment(1);
            if cut? != Cutover::Done {
                return Ok(false);
            }
            volumes.stop_source(id, role).await?;
            tracing::info!(sandbox_id = %id, %role, "disk cut over to its destination");
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_disk_waits_twice_as_long_each_time_up_to_the_cap() {
        let mut backoff = Backoff::default();
        let role = DiskRole::Root;
        let mut tick = 0;
        let mut waits = Vec::new();
        for _ in 0..8 {
            backoff.record(role, tick, true, 60);
            let next = (tick..).find(|t| backoff.due(role, *t)).unwrap();
            waits.push(next - tick);
            tick = next;
        }
        assert_eq!(waits, vec![1, 2, 4, 8, 16, 32, 60, 60]);
        assert!(backoff.due(DiskRole::Swap, tick), "each disk waits alone");
        backoff.record(role, tick, false, 60);
        assert!(backoff.due(role, tick), "a success clears the wait");
    }
}
