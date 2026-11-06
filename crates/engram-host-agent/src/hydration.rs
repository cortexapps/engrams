//! The hydration of a sandbox's cloned disks (ADR 0127 Decisions 5-7):
//! one task per sandbox keeps each source serving and cuts each disk
//! over to its destination once it is copied. The task belongs to the
//! sandbox: `destroy` stops it, and after a host-agent restart it starts
//! again for a live sandbox with a disk left, from the bitmap the kernel
//! kept. A source failure never ends the sandbox: the task logs and tries
//! again on its next tick.

use std::io;
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

/// The hydration task of each sandbox that has one.
pub(crate) struct Hydrations {
    tasks: DashMap<SandboxId, AbortTaskOnDrop>,
    interval: Duration,
}

impl Hydrations {
    pub(crate) fn new(interval: Duration) -> Arc<Self> {
        Arc::new(Self {
            tasks: DashMap::new(),
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
        let owner: Weak<Self> = Arc::downgrade(self);
        let interval = self.interval;
        slot.insert(AbortTaskOnDrop(tokio::spawn(async move {
            hydrate(&volumes, &capture_locks, id, interval).await;
            // Only this task's entry can be here: `arm` fills a vacant
            // slot only, and `disarm` aborts the task as it removes it.
            if let Some(owner) = owner.upgrade() {
                owner.tasks.remove(&id);
                owner.publish_gauge();
            }
        })));
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
    let mut reported: Vec<String> = Vec::new();
    loop {
        ticker.tick().await;
        let pass = pass(volumes, capture_locks, id).await;
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

async fn pass(volumes: &SessionVolumes, capture_locks: &CaptureLocks, id: SandboxId) -> Pass {
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
        match step(volumes, capture_locks, id, role).await {
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
            let lock = capture_lock_in(capture_locks, id);
            let _held = lock.lock().await;
            let cut = volumes.cut_over(id, role).await;
            let outcome = match &cut {
                Ok(Cutover::Done) => "done",
                Ok(Cutover::Waiting(Waiting::Checkpoint)) => "checkpoint_in_flight",
                Ok(Cutover::Waiting(Waiting::Copying(_))) => "copying",
                Err(_) => "error",
            };
            metrics::counter!(crate::metrics::DISK_CUTOVERS_TOTAL, "outcome" => outcome)
                .increment(1);
            let done = cut? == Cutover::Done;
            if done {
                tracing::info!(sandbox_id = %id, %role, "disk cut over to its destination");
            }
            Ok(done)
        }
    }
}
