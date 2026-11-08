//! A limit on chunk transfers in flight, granted in class order.
//!
//! One [`Admission`] per host admits every object-store GET and PUT of a
//! chunk, so downloads and uploads share one limit on the NIC.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::oneshot;

/// Why a chunk moves. A transfer of a lower class is never granted while
/// one of a higher class waits; transfers already in flight are not
/// cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Class {
    /// A guest waits for it now (a page fault, a disk read, a disk the boot
    /// waits for).
    Demand = 0,
    /// Needed soon on the critical path (the pages a resume touches first,
    /// the upload an eviction waits for).
    Prefix = 1,
    /// The rest of a restore, after the critical path.
    Hydrate = 2,
    /// Work no session waits for (staging, periodic checkpoints, cache
    /// warming).
    Background = 3,
}

impl Class {
    pub const ALL: [Class; 4] = [
        Class::Demand,
        Class::Prefix,
        Class::Hydrate,
        Class::Background,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Class::Demand => "demand",
            Class::Prefix => "prefix",
            Class::Hydrate => "hydrate",
            Class::Background => "background",
        }
    }
}

/// Grants permits under one limit. `Demand` may also use `demand_reserve`
/// slots above the limit, so a pipe full of bulk transfers delays a fault
/// by at most one completion. `Hydrate` and `Background` start only while
/// fewer than `bulk_cap` transfers of any class are in flight, so a bulk
/// fill cannot take the link, the CPU and the disk from a restore's
/// critical path. Within a class, permits are granted in arrival order.
pub struct Admission {
    name: &'static str,
    demand_reserve: usize,
    bulk_cap: usize,
    state: Mutex<State>,
}

struct State {
    in_flight: usize,
    limit: usize,
    waiting: [VecDeque<oneshot::Sender<Permit>>; 4],
    /// `None` for a fixed limit.
    tuner: Option<Tuner>,
}

/// One transfer in flight. Dropping it frees the slot; [`Permit::done`]
/// and [`Permit::failed`] also report the outcome to an adaptive limit.
pub struct Permit {
    admission: Arc<Admission>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.admission.release();
    }
}

impl Permit {
    /// The transfer moved `bytes`.
    pub fn done(self, bytes: u64) {
        self.admission.record(bytes, false);
    }

    /// The transfer failed (an error or a timeout from the store).
    pub fn failed(self) {
        self.admission.record(0, true);
    }

    /// Report a store transfer: the bytes it moved, or its error. Only an
    /// error that can mean overload ([`is_overload`]) counts as a failure.
    pub fn finish(self, outcome: std::result::Result<u64, &crate::ChunkStoreError>) {
        match outcome {
            Ok(bytes) => self.done(bytes),
            Err(crate::ChunkStoreError::Blob(e)) if is_overload(e) => self.failed(),
            Err(_) => {}
        }
    }
}

/// Whether a store error can mean the link or the store is overloaded:
/// an SDK, I/O or protocol error (a timeout arrives as an SDK error). A
/// missing object or a bad configuration cannot.
pub(crate) fn is_overload(e: &engram_core::error::BlobError) -> bool {
    use engram_core::error::BlobError as B;
    matches!(e, B::Sdk(_) | B::Io(_) | B::Protocol(_))
}

impl Admission {
    /// A fixed limit. `name` labels the metrics.
    pub fn fixed(name: &'static str, limit: usize, demand_reserve: usize) -> Arc<Self> {
        Self::build(name, limit.max(1), demand_reserve, usize::MAX, None)
    }

    /// A limit that starts at `start` and moves between `min` and `max`
    /// with the measured throughput (see [`Tuner`]). Bulk classes use at
    /// most `bulk_cap` slots of it.
    pub fn adaptive(
        name: &'static str,
        start: usize,
        min: usize,
        max: usize,
        demand_reserve: usize,
        bulk_cap: usize,
    ) -> Arc<Self> {
        let max = max.max(1);
        let min = min.clamp(1, max);
        let start = start.clamp(min, max);
        Self::build(
            name,
            start,
            demand_reserve,
            bulk_cap.max(1),
            Some(Tuner::new(min, max, crate::time_source::metrics_now())),
        )
    }

    fn build(
        name: &'static str,
        limit: usize,
        demand_reserve: usize,
        bulk_cap: usize,
        tuner: Option<Tuner>,
    ) -> Arc<Self> {
        metrics::gauge!("engram_admission_limit", "admission" => name).set(limit as f64);
        Arc::new(Self {
            name,
            demand_reserve,
            bulk_cap,
            state: Mutex::new(State {
                in_flight: 0,
                limit,
                waiting: Default::default(),
                tuner,
            }),
        })
    }

    fn cap(&self, limit: usize, class: Class) -> usize {
        match class {
            Class::Demand => limit + self.demand_reserve,
            Class::Prefix => limit,
            Class::Hydrate | Class::Background => limit.min(self.bulk_cap),
        }
    }

    pub fn in_flight(&self) -> usize {
        self.state.lock().in_flight
    }

    /// The current limit for classes other than `Demand`.
    pub fn limit(&self) -> usize {
        self.state.lock().limit
    }

    pub async fn acquire(self: &Arc<Self>, class: Class) -> Permit {
        let started = crate::time_source::metrics_now();
        let rx = {
            let mut st = self.state.lock();
            let ahead = st.waiting[..=class as usize].iter().any(|q| !q.is_empty());
            if !ahead && st.in_flight < self.cap(st.limit, class) {
                st.in_flight += 1;
                return Permit {
                    admission: Arc::clone(self),
                };
            }
            // Only a wait at the limit asks for a higher limit: a bulk
            // wait at `bulk_cap` stays the same at any limit.
            if st.in_flight >= st.limit {
                if let Some(t) = st.tuner.as_mut() {
                    t.window.saturated = true;
                }
            }
            let (tx, rx) = oneshot::channel();
            st.waiting[class as usize].push_back(tx);
            rx
        };
        // The granter sends the permit itself, so a waiter dropped after the
        // grant drops the permit with the channel and frees the slot.
        let permit = rx.await.expect("admission never drops a waiter");
        metrics::histogram!(
            "engram_admission_wait_seconds",
            "admission" => self.name,
            "class" => class.as_str(),
        )
        .record(started.elapsed().as_secs_f64());
        permit
    }

    fn record(&self, bytes: u64, failed: bool) {
        let mut st = self.state.lock();
        let current = st.limit;
        let Some(tuner) = st.tuner.as_mut() else {
            return;
        };
        let now = crate::time_source::metrics_now();
        let Some(limit) = tuner.record(current, bytes, failed, now) else {
            return;
        };
        if limit != st.limit {
            st.limit = limit;
            metrics::gauge!("engram_admission_limit", "admission" => self.name).set(limit as f64);
        }
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
                if st.in_flight >= self.cap(st.limit, class) {
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

/// How long a measurement window lasts, and the completions it needs
/// before it can close.
const WINDOW: Duration = Duration::from_millis(500);
const WINDOW_COMPLETIONS: u32 = 8;

/// A window older than this when it would close is dropped.
const STALE: Duration = Duration::from_secs(2);

/// An increase must raise throughput by this factor to be kept.
const GAIN: f64 = 1.05;

/// At an unchanged limit, throughput below this factor of the last window
/// means each transfer got slower: back off.
const SLOWDOWN: f64 = 0.8;

/// Windows to hold the limit after an increase did not pay.
const SETTLE_WINDOWS: u32 = 4;

/// A hill climb on throughput. Each window it compares bytes per second
/// with the window before. While transfers wait for slots (the limit is
/// the constraint), it raises the limit as long as throughput grows, and
/// undoes an increase that did not pay. An error, or a throughput drop at
/// an unchanged limit (the latency of each transfer grew), cuts the limit
/// by a quarter, then holds it for [`SETTLE_WINDOWS`] windows: a burst of
/// errors cuts it once. A window with no waiters says nothing about
/// capacity and changes nothing.
struct Tuner {
    min: usize,
    max: usize,
    window: Window,
    /// Throughput of the last window that was saturated, bytes/s.
    last_rate: Option<f64>,
    last_step: Step,
    settle: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Up(usize),
    Hold,
}

struct Window {
    started: Instant,
    bytes: u64,
    completions: u32,
    failed: bool,
    saturated: bool,
}

impl Window {
    fn new(started: Instant) -> Self {
        Self {
            started,
            bytes: 0,
            completions: 0,
            failed: false,
            saturated: false,
        }
    }
}

impl Tuner {
    fn new(min: usize, max: usize, now: Instant) -> Self {
        Self {
            min,
            max,
            window: Window::new(now),
            last_rate: None,
            last_step: Step::Hold,
            settle: 0,
        }
    }

    /// Count one completion. When that closes the window, the new limit
    /// (which can equal `limit`).
    fn record(&mut self, limit: usize, bytes: u64, failed: bool, now: Instant) -> Option<usize> {
        let w = &mut self.window;
        w.bytes += bytes;
        w.completions += 1;
        w.failed |= failed;
        let elapsed = now.saturating_duration_since(w.started);
        // A failure closes the window at once, except while the limit
        // settles after a cut: then it waits for a full window.
        if !(w.failed && self.settle == 0) {
            if elapsed > STALE {
                // A window this long spans idle time: its rate is no
                // measure of capacity.
                self.window = Window::new(now);
                return None;
            }
            if elapsed < WINDOW || w.completions < WINDOW_COMPLETIONS {
                return None;
            }
        }
        let rate = w.bytes as f64 / elapsed.as_secs_f64().max(1e-3);
        let (failed, saturated) = (w.failed, w.saturated);
        self.window = Window::new(now);
        Some(
            self.decide(limit, rate, failed, saturated)
                .clamp(self.min, self.max),
        )
    }

    fn decide(&mut self, limit: usize, rate: f64, failed: bool, saturated: bool) -> usize {
        let step = std::mem::replace(&mut self.last_step, Step::Hold);
        if failed && self.settle > 0 {
            self.settle -= 1;
            return limit;
        }
        if failed {
            return self.lower(limit / 4, limit);
        }
        if !saturated {
            return limit;
        }
        let last = self.last_rate.replace(rate);
        match (last, step) {
            (Some(last), Step::Up(by)) if rate < last * GAIN => self.lower(by, limit),
            (Some(last), Step::Hold) if rate < last * SLOWDOWN => self.lower(limit / 4, limit),
            _ if self.settle > 0 => {
                self.settle -= 1;
                limit
            }
            _ if limit >= self.max => limit,
            _ => {
                let by = (limit / 8).max(1);
                self.last_step = Step::Up(by);
                limit + by
            }
        }
    }

    /// Lower the limit by `by`, then hold it for a few windows. The next
    /// window starts a new baseline: a lower limit lowers throughput too.
    fn lower(&mut self, by: usize, limit: usize) -> usize {
        self.last_rate = None;
        self.settle = SETTLE_WINDOWS;
        limit.saturating_sub(by.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn grants_up_to_the_limit_then_queues() {
        let adm = Admission::fixed("test", 2, 0);
        let a = adm.acquire(Class::Hydrate).await;
        let _b = adm.acquire(Class::Hydrate).await;
        let mut c = Box::pin(adm.acquire(Class::Hydrate));
        assert!(poll_once(&mut c).is_none());
        drop(a);
        let _c = poll_once(&mut c).expect("granted when a slot frees");
        assert_eq!(adm.in_flight(), 2);
    }

    #[tokio::test]
    async fn a_waiting_higher_class_is_granted_first() {
        let adm = Admission::fixed("test", 1, 0);
        let held = adm.acquire(Class::Background).await;
        let mut bulk = Box::pin(adm.acquire(Class::Hydrate));
        let mut prefix = Box::pin(adm.acquire(Class::Prefix));
        assert!(poll_once(&mut bulk).is_none());
        assert!(poll_once(&mut prefix).is_none());
        drop(held);
        assert!(poll_once(&mut bulk).is_none(), "bulk waits behind prefix");
        let p = poll_once(&mut prefix).expect("prefix granted first");
        drop(p);
        assert!(poll_once(&mut bulk).is_some());
    }

    #[tokio::test]
    async fn demand_uses_the_reserve_when_the_limit_is_full() {
        let adm = Admission::fixed("test", 1, 1);
        let _bulk = adm.acquire(Class::Hydrate).await;
        let mut more_bulk = Box::pin(adm.acquire(Class::Hydrate));
        assert!(poll_once(&mut more_bulk).is_none());
        let _fault = adm.acquire(Class::Demand).await;
        assert_eq!(adm.in_flight(), 2);
    }

    #[tokio::test]
    async fn bulk_stops_at_its_cap_while_prefix_uses_the_whole_limit() {
        let adm = Admission::adaptive("test", 3, 1, 4, 0, 1);
        let saturated = || adm.state.lock().tuner.as_ref().unwrap().window.saturated;
        let _bulk = adm.acquire(Class::Hydrate).await;
        let mut more_bulk = Box::pin(adm.acquire(Class::Background));
        assert!(poll_once(&mut more_bulk).is_none(), "bulk is at its cap");
        assert!(!saturated(), "a higher limit would not start the bulk wait");
        let _a = adm.acquire(Class::Prefix).await;
        let _b = adm.acquire(Class::Prefix).await;
        assert_eq!(adm.in_flight(), 3);
        let mut more_prefix = Box::pin(adm.acquire(Class::Prefix));
        assert!(
            poll_once(&mut more_prefix).is_none(),
            "prefix is at the limit"
        );
        assert!(saturated(), "a wait at the limit asks for a higher one");
    }

    #[tokio::test]
    async fn a_dropped_waiter_does_not_leak_its_slot() {
        let adm = Admission::fixed("test", 1, 0);
        let held = adm.acquire(Class::Hydrate).await;
        let mut gone = Box::pin(adm.acquire(Class::Hydrate));
        assert!(poll_once(&mut gone).is_none());
        drop(held);
        // Granted to `gone`, which is dropped before it reads the permit.
        drop(gone);
        assert_eq!(adm.in_flight(), 0);
        let _next = adm.acquire(Class::Hydrate).await;
    }

    #[tokio::test]
    async fn a_reported_outcome_frees_the_slot() {
        let adm = Admission::adaptive("test", 1, 1, 4, 0, usize::MAX);
        adm.acquire(Class::Hydrate).await.done(10);
        adm.acquire(Class::Hydrate).await.failed();
        assert_eq!(adm.in_flight(), 0);
    }

    /// Poll a future once; `Some` when it completed.
    fn poll_once<F: std::future::Future + Unpin>(f: &mut F) -> Option<F::Output> {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        match std::pin::Pin::new(f).poll(&mut cx) {
            std::task::Poll::Ready(v) => Some(v),
            std::task::Poll::Pending => None,
        }
    }

    /// Drives a tuner through windows of a modelled link.
    struct Link {
        tuner: Tuner,
        limit: usize,
        now: Instant,
    }

    impl Link {
        fn new(start: usize, min: usize, max: usize) -> Self {
            let now = crate::time_source::metrics_now();
            Self {
                tuner: Tuner::new(min, max, now),
                limit: start,
                now,
            }
        }

        /// One window that moves `rate` bytes per second; the limit after it.
        fn window(&mut self, rate: f64, saturated: bool, failed: bool) -> usize {
            self.tuner.window.saturated = saturated;
            let per = rate * WINDOW.as_secs_f64() / f64::from(WINDOW_COMPLETIONS);
            for i in 1..=WINDOW_COMPLETIONS {
                let last = i == WINDOW_COMPLETIONS;
                if last {
                    self.now += WINDOW;
                }
                let failed = failed && last;
                if let Some(l) = self.tuner.record(self.limit, per as u64, failed, self.now) {
                    self.limit = l;
                }
            }
            self.limit
        }

        /// `windows` saturated windows, each moving `rate(limit)`.
        fn run(&mut self, windows: usize, rate: impl Fn(usize) -> f64) -> usize {
            for _ in 0..windows {
                self.window(rate(self.limit), true, false);
            }
            self.limit
        }
    }

    /// Throughput grows with the limit up to `knee` slots, then stays flat.
    fn knee(knee: usize) -> impl Fn(usize) -> f64 {
        move |limit| limit.min(knee) as f64 * 1e6
    }

    #[test]
    fn the_limit_grows_while_throughput_grows_and_stops_near_the_knee() {
        let mut link = Link::new(16, 4, 512);
        let limit = link.run(80, knee(100));
        assert!(
            (90..=130).contains(&limit),
            "limit {limit} should settle near the knee at 100"
        );
    }

    #[test]
    fn the_limit_never_passes_the_ceiling() {
        let mut link = Link::new(16, 4, 40);
        assert_eq!(link.run(60, knee(1000)), 40);
    }

    #[test]
    fn an_error_cuts_the_limit_by_a_quarter() {
        let mut link = Link::new(64, 4, 256);
        assert_eq!(link.window(64e6, true, true), 48);
    }

    #[test]
    fn a_burst_of_errors_cuts_the_limit_once() {
        let mut link = Link::new(64, 4, 256);
        for _ in 0..10 {
            if let Some(l) = link.tuner.record(link.limit, 0, true, link.now) {
                link.limit = l;
            }
        }
        assert_eq!(link.limit, 48);
    }

    #[test]
    fn errors_that_go_on_cut_again_after_the_limit_settles() {
        let mut link = Link::new(64, 4, 256);
        let limits: Vec<usize> = (0..=SETTLE_WINDOWS + 1)
            .map(|_| link.window(64e6, true, true))
            .collect();
        assert_eq!(limits, [48, 48, 48, 48, 48, 36]);
    }

    #[test]
    fn a_throughput_drop_at_a_steady_limit_backs_off() {
        let mut link = Link::new(64, 4, 64);
        assert_eq!(
            link.run(3, knee(1000)),
            64,
            "at the ceiling, the limit holds"
        );
        // Each transfer now takes twice as long.
        assert_eq!(link.window(32e6, true, false), 48);
    }

    #[test]
    fn windows_without_waiters_change_nothing() {
        let mut link = Link::new(64, 4, 256);
        for _ in 0..10 {
            assert_eq!(link.window(1e3, false, false), 64);
        }
    }

    #[test]
    fn a_window_that_spans_idle_time_is_dropped() {
        let mut link = Link::new(64, 4, 256);
        link.now += STALE * 10;
        assert_eq!(link.tuner.record(64, 1, false, link.now), None);
        assert_eq!(link.tuner.window.completions, 0, "a new window starts");
    }
}
