//! The timer every periodic background loop uses.

use std::time::Duration;

use tokio::time::{Interval, MissedTickBehavior};

/// A `tokio` interval of `period` (the first tick completes at once) that
/// waits a full `period` after a late tick. The `tokio` default fires
/// every missed tick back to back, so a loop whose step ran long would
/// then run many steps with no gap. Panics if `period` is zero.
pub fn periodic(period: Duration) -> Interval {
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}
