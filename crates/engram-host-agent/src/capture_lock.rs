//! The per-sandbox capture locks and the `host.capture_lock_wait` span
//! that times each acquisition.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use engram_core::SandboxId;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::Instrument as _;

/// The operation that takes a capture lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum CaptureUser {
    PeriodicCheckpoint = 1,
    SnapshotRpc,
    EvictionCapture,
    HeldCapture,
    DeferredSnapshot,
    PostCopyCapture,
    StartAgent,
    ChainRehydrate,
    FinalizeRedrive,
    ShutdownDrain,
}

impl CaptureUser {
    const ALL: [CaptureUser; 10] = [
        CaptureUser::PeriodicCheckpoint,
        CaptureUser::SnapshotRpc,
        CaptureUser::EvictionCapture,
        CaptureUser::HeldCapture,
        CaptureUser::DeferredSnapshot,
        CaptureUser::PostCopyCapture,
        CaptureUser::StartAgent,
        CaptureUser::ChainRehydrate,
        CaptureUser::FinalizeRedrive,
        CaptureUser::ShutdownDrain,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CaptureUser::PeriodicCheckpoint => "periodic_checkpoint",
            CaptureUser::SnapshotRpc => "snapshot_rpc",
            CaptureUser::EvictionCapture => "eviction_capture",
            CaptureUser::HeldCapture => "held_capture",
            CaptureUser::DeferredSnapshot => "deferred_snapshot",
            CaptureUser::PostCopyCapture => "post_copy_capture",
            CaptureUser::StartAgent => "start_agent",
            CaptureUser::ChainRehydrate => "chain_rehydrate",
            CaptureUser::FinalizeRedrive => "finalize_redrive",
            CaptureUser::ShutdownDrain => "shutdown_drain",
        }
    }

    fn from_u8(v: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|u| *u as u8 == v)
    }
}

/// One lock of the gate, with the last user that took it.
#[derive(Default)]
struct TrackedLock {
    mutex: Arc<Mutex<()>>,
    /// The last user that took the lock, so its holder while it is held.
    holder: AtomicU8,
}

impl TrackedLock {
    async fn acquire(
        &self,
        sandbox_id: SandboxId,
        lock: &'static str,
        user: CaptureUser,
    ) -> OwnedMutexGuard<()> {
        let span = tracing::info_span!(
            "host.capture_lock_wait",
            %sandbox_id,
            lock,
            waiter = user.as_str(),
            holder = tracing::field::Empty,
        );
        let guard = match Arc::clone(&self.mutex).try_lock_owned() {
            Ok(guard) => guard,
            Err(_) => {
                let holder = CaptureUser::from_u8(self.holder.load(Ordering::Relaxed));
                span.record("holder", holder.map_or("unknown", CaptureUser::as_str));
                Arc::clone(&self.mutex).lock_owned().instrument(span).await
            }
        };
        self.holder.store(user as u8, Ordering::Relaxed);
        guard
    }

    fn is_held(&self) -> bool {
        self.mutex.try_lock().is_err()
    }
}

/// The per-sandbox capture locks. Take `serial` before `quiesce`.
#[derive(Default)]
pub(crate) struct CaptureGate {
    /// The capture lock: serializes captures, held through the post phase.
    serial: TrackedLock,
    /// Held by a capture from before its pause until its guest resumes,
    /// and by `start_agent` (ADR 0123 D4: a `SpawnHarness`, which arms
    /// swap, never straddles a pause). The post phase reads only what the
    /// pause froze — the disk snapshots, the FC staging files, the chain
    /// head cloned under `serial` — so it runs without this lock.
    quiesce: TrackedLock,
}

impl CaptureGate {
    pub(crate) async fn serialize(
        &self,
        sandbox_id: SandboxId,
        user: CaptureUser,
    ) -> OwnedMutexGuard<()> {
        self.serial.acquire(sandbox_id, "serial", user).await
    }

    pub(crate) async fn quiesce(
        &self,
        sandbox_id: SandboxId,
        user: CaptureUser,
    ) -> OwnedMutexGuard<()> {
        self.quiesce.acquire(sandbox_id, "quiesce", user).await
    }

    /// Whether a capture or a harness spawn holds either lock.
    pub(crate) fn in_flight(&self) -> bool {
        self.serial.is_held() || self.quiesce.is_held()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_user_round_trips_through_its_tag() {
        for user in CaptureUser::ALL {
            assert_eq!(CaptureUser::from_u8(user as u8), Some(user), "{user:?}");
        }
        assert_eq!(CaptureUser::from_u8(0), None);
    }

    #[tokio::test]
    async fn a_waiter_takes_the_lock_after_the_holder_releases_it() {
        let gate = Arc::new(CaptureGate::default());
        let id = SandboxId::new();
        let held = gate.serialize(id, CaptureUser::PeriodicCheckpoint).await;
        assert!(gate.in_flight());

        let waiter = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move {
                drop(gate.serialize(id, CaptureUser::SnapshotRpc).await);
            }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(held);
        waiter.await.unwrap();
        assert!(!gate.in_flight());
        assert_eq!(
            CaptureUser::from_u8(gate.serial.holder.load(Ordering::Relaxed)),
            Some(CaptureUser::SnapshotRpc)
        );
    }

    #[tokio::test]
    async fn the_two_locks_are_independent() {
        let gate = CaptureGate::default();
        let id = SandboxId::new();
        let _serial = gate.serialize(id, CaptureUser::EvictionCapture).await;
        drop(gate.quiesce(id, CaptureUser::StartAgent).await);
        assert!(gate.in_flight());
    }
}
