//! Honest monotonic-clock carve-out for the unsimulated host operator.
//!
//! These reads drive kubectl-roll and deletion budget deadlines. The operator
//! has no simulation clock seam, so ADR 0098 P1's `metrics_now` pattern keeps
//! the real-clock contract isolated in one reviewed location.

/// Read the monotonic clock for operator deadline budgets.
#[allow(clippy::disallowed_methods)] // unsimulated operator deadline budget (ADR 0098 P1)
pub(crate) fn metrics_now_tokio() -> tokio::time::Instant {
    tokio::time::Instant::now()
}
