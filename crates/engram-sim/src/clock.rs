//! The simulated clock: one time source, borrowed from tokio.
//!
//! Per ADR 0098, `SimClock` does NOT keep its own counter — it is a view
//! over tokio's paused clock (`base_utc + skew + (tokio Instant − birth)`),
//! so `sleep()`s, `timeout()`s, and `now_utc()` all move in lockstep when
//! the scheduler calls [`SimClock::advance`]. It therefore requires a
//! runtime with paused time (`#[tokio::test(start_paused = true)]`, or
//! `Builder::new_current_thread().enable_time()` + `tokio::time::pause()`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use engram_core::traits::Clock;
use parking_lot::Mutex;

/// Deterministic UTC epoch every simulation starts at unless overridden:
/// 2026-01-01T00:00:00Z. Fixed (not "now") so failure traces are
/// comparable across runs and machines.
pub const SIM_EPOCH_UNIX: i64 = 1_767_225_600;

#[derive(Debug)]
pub struct SimClock {
    base_utc: DateTime<Utc>,
    birth: tokio::time::Instant,
    /// Per-replica skew, mutated by the (future, D5/D6) clock-skew fault.
    skew: Mutex<chrono::Duration>,
}

impl SimClock {
    /// Must be called inside a tokio runtime whose clock is (or will be)
    /// paused; `birth` pins the virtual origin.
    pub fn new() -> Arc<Self> {
        Self::at(DateTime::from_timestamp(SIM_EPOCH_UNIX, 0).expect("valid sim epoch"))
    }

    pub fn at(base_utc: DateTime<Utc>) -> Arc<Self> {
        Arc::new(Self {
            base_utc,
            birth: tokio::time::Instant::now(),
            skew: Mutex::new(chrono::Duration::zero()),
        })
    }

    /// Advance virtual time. Fires every due tokio sleep/timeout in the
    /// runtime as a side effect — which is the point.
    pub async fn advance(&self, dur: Duration) {
        tokio::time::advance(dur).await;
    }

    /// Perturb this clock's wall-clock view relative to its peers
    /// (a clock-skew fault). Monotonic time is unaffected — skew models
    /// wall-clock disagreement, not time travel.
    pub fn set_skew(&self, skew: chrono::Duration) {
        *self.skew.lock() = skew;
    }
}

impl Clock for SimClock {
    fn now_utc(&self) -> DateTime<Utc> {
        let elapsed = self.birth.elapsed();
        self.base_utc
            + chrono::Duration::from_std(elapsed).unwrap_or_else(|_| chrono::Duration::zero())
            + *self.skew.lock()
    }

    fn now_mono(&self) -> Duration {
        self.birth.elapsed()
    }

    fn sleep(&self, dur: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep(dur))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn time_moves_only_when_advanced() {
        let clock = SimClock::new();
        let t0 = clock.now_utc();
        // Real time passing does nothing under a paused runtime.
        assert_eq!(clock.now_utc(), t0);
        clock.advance(Duration::from_secs(90)).await;
        assert_eq!((clock.now_utc() - t0).num_seconds(), 90);
        assert_eq!(clock.now_mono(), Duration::from_secs(90));
    }

    #[tokio::test(start_paused = true)]
    async fn advance_fires_due_sleeps() {
        let clock = SimClock::new();
        let sleep = clock.sleep(Duration::from_secs(10));
        tokio::pin!(sleep);
        // Not yet due.
        assert!(
            futures_poll_once(sleep.as_mut()).await.is_none(),
            "sleep must be pending before advance"
        );
        clock.advance(Duration::from_secs(10)).await;
        sleep.await; // resolves without real time passing
    }

    #[tokio::test(start_paused = true)]
    async fn skew_offsets_wall_clock_not_mono() {
        let clock = SimClock::new();
        let t0 = clock.now_utc();
        clock.set_skew(chrono::Duration::seconds(-30));
        assert_eq!((t0 - clock.now_utc()).num_seconds(), 30);
        assert_eq!(clock.now_mono(), Duration::ZERO);
    }

    async fn futures_poll_once<F: Future>(f: Pin<&mut F>) -> Option<F::Output> {
        struct Once<'a, F>(Option<Pin<&'a mut F>>);
        impl<F: Future> Future for Once<'_, F> {
            type Output = Option<F::Output>;
            fn poll(
                mut self: Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Self::Output> {
                let inner = self.0.take().expect("polled after ready");
                match inner.poll(cx) {
                    std::task::Poll::Ready(v) => std::task::Poll::Ready(Some(v)),
                    std::task::Poll::Pending => std::task::Poll::Ready(None),
                }
            }
        }
        Once(Some(f)).await
    }
}

/// A clock advanced synchronously by the test — no tokio pausing
/// involved. The conformance suite uses this for BOTH stores: driving
/// real Postgres I/O under a paused tokio runtime trips auto-advance
/// on the sqlx pool's internal timers, so the suite keeps the runtime
/// real and moves only the *logical* clock.
#[derive(Debug)]
pub struct ManualClock {
    base_utc: DateTime<Utc>,
    offset: Mutex<chrono::Duration>,
}

impl ManualClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            base_utc: DateTime::from_timestamp(SIM_EPOCH_UNIX, 0).expect("valid sim epoch"),
            offset: Mutex::new(chrono::Duration::zero()),
        })
    }

    pub fn advance(&self, dur: Duration) {
        *self.offset.lock() +=
            chrono::Duration::from_std(dur).unwrap_or_else(|_| chrono::Duration::zero());
    }
}

impl Clock for ManualClock {
    fn now_utc(&self) -> DateTime<Utc> {
        self.base_utc + *self.offset.lock()
    }

    fn now_mono(&self) -> Duration {
        self.offset.lock().to_std().unwrap_or(Duration::ZERO)
    }

    fn sleep(&self, dur: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep(dur))
    }
}
