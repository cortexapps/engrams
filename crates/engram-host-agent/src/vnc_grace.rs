//! ADR 0064: cancellable grace timers for ephemeral browser teardown. A VNC
//! viewer disconnect schedules `stop_browser` after a grace; a reconnect within
//! the grace cancels it so a live viewer is never killed. A per-entry
//! generation token lets a fired timer self-clean only when it is still the
//! live one — so the map never leaks entries across sandboxes.
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::task::AbortHandle;

use engram_core::SandboxId;

#[derive(Clone, Default)]
pub(crate) struct VncGrace {
    timers: Arc<DashMap<SandboxId, (u64, AbortHandle)>>,
    generation: Arc<AtomicU64>,
}

impl VncGrace {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Cancel any pending teardown for this sandbox (called on viewer connect).
    pub(crate) fn cancel(&self, id: SandboxId) {
        if let Some((_, (_, handle))) = self.timers.remove(&id) {
            handle.abort();
        }
    }

    /// Schedule `action` to run after `grace` unless a later `cancel`/`schedule`
    /// supersedes it (called on viewer disconnect).
    pub(crate) fn schedule<F, Fut>(&self, id: SandboxId, grace: Duration, action: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let my_gen = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let timers = self.timers.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            action().await;
            // Self-clean only if we are still the live timer for this sandbox.
            timers.remove_if(&id, |_, (g, _)| *g == my_gen);
        });
        self.timers.insert(id, (my_gen, task.abort_handle()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn test_id() -> SandboxId {
        SandboxId::new()
    }

    #[tokio::test]
    async fn cancel_prevents_teardown() {
        let grace = VncGrace::new();
        let id = test_id();
        let fired = Arc::new(AtomicBool::new(false));
        let f = fired.clone();
        grace.schedule(id, Duration::from_millis(100), move || {
            let f = f.clone();
            async move {
                f.store(true, Ordering::SeqCst);
            }
        });
        grace.cancel(id); // reconnect lands before the grace elapses
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            !fired.load(Ordering::SeqCst),
            "cancel must prevent the scheduled teardown"
        );
    }

    #[tokio::test]
    async fn schedule_fires_without_cancel() {
        let grace = VncGrace::new();
        let id = test_id();
        let fired = Arc::new(AtomicBool::new(false));
        let f = fired.clone();
        grace.schedule(id, Duration::from_millis(100), move || {
            let f = f.clone();
            async move {
                f.store(true, Ordering::SeqCst);
            }
        });
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            fired.load(Ordering::SeqCst),
            "no cancel → teardown fires"
        );
    }
}
