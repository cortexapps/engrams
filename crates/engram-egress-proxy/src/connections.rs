//! A stable session gate. All socket futures must remain owned by the lease.
use parking_lot::Mutex;
use std::{future::Future, sync::Arc};
use tokio::sync::{watch, Notify};

pub(crate) struct Connections {
    state: Mutex<State>,
    pub(crate) apply: tokio::sync::Mutex<()>,
    drained: Notify,
}
struct State {
    open: bool,
    poisoned: bool,
    active: usize,
    cancel: watch::Sender<u64>,
}
impl Default for Connections {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                open: true,
                poisoned: false,
                active: 0,
                cancel: watch::channel(0).0,
            }),
            apply: tokio::sync::Mutex::new(()),
            drained: Notify::new(),
        }
    }
}
impl Connections {
    pub(crate) fn enroll(self: &Arc<Self>) -> Option<ConnectionLease> {
        let mut state = self.state.lock();
        if !state.open {
            return None;
        }
        state.active += 1;
        Some(ConnectionLease {
            control: self.clone(),
            cancel: state.cancel.subscribe(),
        })
    }
    pub(crate) fn is_open(&self) -> bool {
        self.state.lock().open
    }
    pub(crate) fn close(&self) {
        let mut state = self.state.lock();
        state.open = false;
        state.cancel.send_modify(|v| *v = v.wrapping_add(1));
    }
    pub(crate) async fn drain(&self) {
        loop {
            let notified = self.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state.lock().active == 0 {
                return;
            }
            notified.await;
        }
    }
    pub(crate) fn is_poisoned(&self) -> bool {
        self.state.lock().poisoned
    }
    pub(crate) fn open(&self) {
        let mut state = self.state.lock();
        if !state.poisoned {
            state.open = true;
        }
    }
}

pub struct ConnectionLease {
    control: Arc<Connections>,
    cancel: watch::Receiver<u64>,
}
impl ConnectionLease {
    /// A failed private cleanup barrier cannot be repaired by policy replay
    /// in this proxy incarnation. Retain the closed session gate.
    pub fn poison(&self) {
        let mut state = self.control.state.lock();
        state.poisoned = true;
        state.open = false;
        state.cancel.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Allocation owners observe cancellation, close admission, then join all
    /// child work before dropping this lease and releasing a policy drain.
    pub async fn cancelled(&mut self) {
        let _ = self.cancel.changed().await;
    }

    /// Drop the complete socket future before releasing the drain guard.
    pub async fn run<F: Future>(mut self, future: F) -> Option<F::Output> {
        tokio::select! { biased;
            _ = self.cancel.changed() => None,
            result = future => Some(result),
        }
    }
}
impl Drop for ConnectionLease {
    fn drop(&mut self) {
        let mut state = self.control.state.lock();
        state.active -= 1;
        if state.active == 0 {
            self.control.drained.notify_waiters();
        }
    }
}
