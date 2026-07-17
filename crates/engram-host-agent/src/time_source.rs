//! The host-agent's monotonic-clock and path-token carve-outs (ADR 0098
//! Phase 2, P1).
//!
//! ADR 0098 D1 bans raw `Instant::now` / `Uuid::new_v4` crate-wide (see
//! `clippy.toml`); decision-feeding wall-clock and id sites read the
//! injected `clock`/`entropy` instead. The three helpers here are the
//! deliberate, single-`#[allow]` exceptions — each honest, none a
//! decision input the simulator reasons about:
//!
//! - [`metrics_now`] / [`metrics_now_tokio`] — the monotonic-`Instant`
//!   carve-out. Two homes: (a) the data-plane latency histograms and
//!   duration logs (the migrate_peer page-server timers, NBD/attach
//!   budgets, per-stage finalize timers) — the data plane is never
//!   simulated, so a `Box::pin`-per-op `clock.now_mono()` would trade
//!   latency for nothing; and (b) the not-yet-extracted flow timers
//!   (drain/dial/park/adopt deadlines, the peer-health window, the
//!   migration last-activity mark). ADR 0098 Phase 2 converts each flow's
//!   decision timers to `clock.now_mono()` as that flow becomes a pure
//!   `run_once()` step (Flow C→P3, A→P4, D→P5, F→P6, B→P7, E→P8); until
//!   then a raw monotonic read has no simulation surface, so it reads the
//!   real clock here rather than pretending to be injected.
//!
//! - [`unique_path_token`] — a UUID minted purely for filesystem-path
//!   uniqueness (ephemeral temp dirs, per-pull scratch dirs). Not an
//!   identifier that lands in any record or decision, so it is not
//!   entropy the simulator seeds; it stays a real v4 UUID.

/// The single monotonic `std::time::Instant` read for the host-agent. See
/// the module doc for the two justified uses.
#[allow(clippy::disallowed_methods)] // the one honest std Instant::now caller (ADR 0098 Phase 2 P1)
pub(crate) fn metrics_now() -> std::time::Instant {
    std::time::Instant::now()
}

/// The `tokio::time::Instant` twin of [`metrics_now`], for the paused-clock
/// deadline arithmetic in the migrate_peer park loop.
#[allow(clippy::disallowed_methods)] // the one honest tokio Instant::now caller (ADR 0098 Phase 2 P1)
pub(crate) fn metrics_now_tokio() -> tokio::time::Instant {
    tokio::time::Instant::now()
}

/// A v4 UUID for filesystem-path uniqueness only — never a record id or a
/// decision input, so it is not seeded entropy. See the module doc.
#[allow(clippy::disallowed_methods)] // path-uniqueness only, not a simulation input (ADR 0098 Phase 2 P1)
pub(crate) fn unique_path_token() -> uuid::Uuid {
    uuid::Uuid::new_v4()
}
