//! Honest monotonic-clock carve-outs for in-guest readiness deadlines.
//!
//! Agentd runs inside the guest and is never simulated. Its readiness polls
//! use real deadline budgets, isolated here following ADR 0098 P1's
//! `metrics_now` pattern.

/// Read the standard monotonic clock for an in-guest readiness deadline.
/// `pub` (not `pub(crate)`) so the `engram-agentd` binary can call it without
/// recompiling this module — see `lib.rs`.
#[allow(clippy::disallowed_methods)] // in-guest readiness deadline (ADR 0098 P1)
pub fn metrics_now() -> std::time::Instant {
    std::time::Instant::now()
}

/// Read Tokio's monotonic clock for an in-guest readiness deadline.
#[allow(clippy::disallowed_methods)] // in-guest readiness deadline (ADR 0098 P1)
pub(crate) fn metrics_now_tokio() -> tokio::time::Instant {
    tokio::time::Instant::now()
}
