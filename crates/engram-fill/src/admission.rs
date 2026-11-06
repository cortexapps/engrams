//! A limit on store fetches in flight, granted in class order.

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::oneshot;

use crate::Class;

/// Grants permits under one limit. `Demand` may also use `demand_reserve`
/// slots above the limit, so a pipe full of bulk fetches delays a fault by at
/// most one completion. Within a class, permits are granted in arrival order.
pub struct Admission {
    limit: usize,
    demand_reserve: usize,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    in_flight: usize,
    waiting: [VecDeque<oneshot::Sender<Permit>>; 4],
}

/// One fetch in flight. Dropping it frees the slot.
pub struct Permit {
    admission: Arc<Admission>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.admission.release();
    }
}

impl Admission {
    pub fn new(limit: usize, demand_reserve: usize) -> Arc<Self> {
        Arc::new(Self {
            limit: limit.max(1),
            demand_reserve,
            state: Mutex::new(State::default()),
        })
    }

    fn cap(&self, class: Class) -> usize {
        match class {
            Class::Demand => self.limit + self.demand_reserve,
            _ => self.limit,
        }
    }

    pub fn in_flight(&self) -> usize {
        self.state.lock().in_flight
    }

    pub async fn acquire(self: &Arc<Self>, class: Class) -> Permit {
        let started = std::time::Instant::now();
        let rx = {
            let mut st = self.state.lock();
            let ahead = st.waiting[..=class as usize].iter().any(|q| !q.is_empty());
            if !ahead && st.in_flight < self.cap(class) {
                st.in_flight += 1;
                return Permit {
                    admission: Arc::clone(self),
                };
            }
            let (tx, rx) = oneshot::channel();
            st.waiting[class as usize].push_back(tx);
            rx
        };
        // The granter sends the permit itself, so a waiter dropped after the
        // grant drops the permit with the channel and frees the slot.
        let permit = rx.await.expect("admission never drops a waiter");
        metrics::histogram!("engram_fill_admission_wait_seconds", "class" => class.as_str())
            .record(started.elapsed().as_secs_f64());
        permit
    }

    fn release(self: &Arc<Self>) {
        let grants = {
            let mut st = self.state.lock();
            st.in_flight -= 1;
            self.grant_waiting(&mut st)
        };
        // Sent outside the lock: a permit whose waiter is gone comes back
        // here and is dropped, which re-enters `release`.
        for (tx, permit) in grants {
            let _ = tx.send(permit);
        }
    }

    fn grant_waiting(self: &Arc<Self>, st: &mut State) -> Vec<(oneshot::Sender<Permit>, Permit)> {
        let mut grants = Vec::new();
        for class in Class::ALL {
            while !st.waiting[class as usize].is_empty() {
                if st.in_flight >= self.cap(class) {
                    // Every lower class has a cap no larger than this one.
                    return grants;
                }
                let tx = st.waiting[class as usize]
                    .pop_front()
                    .expect("checked non-empty");
                st.in_flight += 1;
                grants.push((
                    tx,
                    Permit {
                        admission: Arc::clone(self),
                    },
                ));
            }
        }
        grants
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn grants_up_to_the_limit_then_queues() {
        let adm = Admission::new(2, 0);
        let a = adm.acquire(Class::Hydrate).await;
        let _b = adm.acquire(Class::Hydrate).await;
        let mut c = Box::pin(adm.acquire(Class::Hydrate));
        assert!(futures_poll(&mut c).is_none());
        drop(a);
        let _c = futures_poll(&mut c).expect("granted when a slot frees");
        assert_eq!(adm.in_flight(), 2);
    }

    #[tokio::test]
    async fn a_waiting_higher_class_is_granted_first() {
        let adm = Admission::new(1, 0);
        let held = adm.acquire(Class::Background).await;
        let mut bulk = Box::pin(adm.acquire(Class::Hydrate));
        let mut prefix = Box::pin(adm.acquire(Class::Prefix));
        assert!(futures_poll(&mut bulk).is_none());
        assert!(futures_poll(&mut prefix).is_none());
        drop(held);
        assert!(
            futures_poll(&mut bulk).is_none(),
            "bulk waits behind prefix"
        );
        let p = futures_poll(&mut prefix).expect("prefix granted first");
        drop(p);
        assert!(futures_poll(&mut bulk).is_some());
    }

    #[tokio::test]
    async fn demand_uses_the_reserve_when_the_limit_is_full() {
        let adm = Admission::new(1, 1);
        let _bulk = adm.acquire(Class::Hydrate).await;
        let mut more_bulk = Box::pin(adm.acquire(Class::Hydrate));
        assert!(futures_poll(&mut more_bulk).is_none());
        let _fault = adm.acquire(Class::Demand).await;
        assert_eq!(adm.in_flight(), 2);
    }

    #[tokio::test]
    async fn a_dropped_waiter_does_not_leak_its_slot() {
        let adm = Admission::new(1, 0);
        let held = adm.acquire(Class::Hydrate).await;
        let mut gone = Box::pin(adm.acquire(Class::Hydrate));
        assert!(futures_poll(&mut gone).is_none());
        drop(held);
        // Granted to `gone`, which is dropped before it reads the permit.
        drop(gone);
        assert_eq!(adm.in_flight(), 0);
        let _next = adm.acquire(Class::Hydrate).await;
    }

    /// Poll a future once; `Some` when it completed.
    fn futures_poll<F: std::future::Future + Unpin>(f: &mut F) -> Option<F::Output> {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        match std::pin::Pin::new(f).poll(&mut cx) {
            std::task::Poll::Ready(v) => Some(v),
            std::task::Poll::Pending => None,
        }
    }
}
