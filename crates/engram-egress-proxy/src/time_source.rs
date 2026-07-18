//! Honest clock carve-outs for the unsimulated egress proxy.
//!
//! Certificate validity and upstream-token freshness are real-wall-clock
//! contracts; SNI connection budgets use a real monotonic deadline. The proxy
//! is not simulated, so ADR 0098 P1's pattern isolates those reads here.

/// Read wall clock for TLS validity and upstream-token freshness.
#[allow(clippy::disallowed_methods)] // real-wall-clock protocol contract (ADR 0098 P1)
pub(crate) fn wall_now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

/// Read Tokio's monotonic clock for an SNI connection budget.
#[allow(clippy::disallowed_methods)] // unsimulated proxy deadline budget (ADR 0098 P1)
pub(crate) fn metrics_now_tokio() -> tokio::time::Instant {
    tokio::time::Instant::now()
}
