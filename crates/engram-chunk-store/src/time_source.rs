//! Honest monotonic-clock carve-out for data-plane latency metrics.
//!
//! Chunk-store timers feed histograms and duration logs, never simulated
//! decisions. ADR 0098 P1's pattern isolates the real clock read here.

/// Read the monotonic clock for a data-plane latency measurement.
#[allow(clippy::disallowed_methods)] // data-plane latency timer (ADR 0098 P1)
pub(crate) fn metrics_now() -> std::time::Instant {
    std::time::Instant::now()
}

/// Read the monotonic clock for the expiry of a process-local memo (the
/// upload memo). The memo only skips repeated work; the state it guards
/// is always correct without it.
#[allow(clippy::disallowed_methods)] // data-plane memo expiry (ADR 0098 P1)
pub(crate) fn memo_now() -> std::time::Instant {
    std::time::Instant::now()
}
