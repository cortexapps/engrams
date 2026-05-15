//! Host-side idle-eviction detection driver (ADR 0011 follow-up #2,
//! landed via ADR 0013).
//!
//! The host's local `HarnessHub` is the authoritative source of
//! "this sandbox's adapter has been quiet for N seconds" — only the
//! host sees every harness event in real-time. In the pre-0013
//! single-replica coord world, the coord polled the hub directly
//! because the hub was in-proc. With a stateless coord, no single
//! pod's local hub is authoritative anymore (events fan in via
//! HTTP POSTs from multiple hosts to multiple coord pods).
//!
//! Resolution: the host runs the *driver* (this module), the coord
//! runs the *pipeline* (`engram_coordinator::idle_evictor::
//! evict_idle_session`). The driver scans the local hub on a tick,
//! finds candidates past TTL, POSTs them to
//! `/api/hosts/:id/idle-eviction-candidates`. The receiving coord
//! pod (any pod) runs the pipeline; the pipeline is idempotent so
//! multiple pods receiving the same batch (or the same candidate
//! batched across two ticks) doesn't cause double-eviction.

use std::time::Duration;

/// Soft idle TTL — a session whose adapter emitted `Idle` and stayed
/// quiet for this long is hot-suspended. Matches the coord-side
/// constant.
pub const DEFAULT_IDLE_TTL_SECS: u64 = 30;

/// Hard idle TTL — backstop for adapters that go silent without ever
/// emitting `Idle` (stuck in a tool call, infinite loop). Matches
/// the coord-side constant.
pub const DEFAULT_IDLE_HARD_TTL_SECS: u64 = 1800;

/// How often the host scans for over-TTL sandboxes.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Read `ENGRAM_IDLE_TTL_SECS` (soft TTL) — falls through to default.
pub fn idle_ttl_from_env() -> Duration {
    std::env::var("ENGRAM_IDLE_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_IDLE_TTL_SECS))
}

/// Read `ENGRAM_IDLE_HARD_TTL_SECS` (hard TTL) — falls through to default.
pub fn idle_hard_ttl_from_env() -> Duration {
    std::env::var("ENGRAM_IDLE_HARD_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_IDLE_HARD_TTL_SECS))
}
