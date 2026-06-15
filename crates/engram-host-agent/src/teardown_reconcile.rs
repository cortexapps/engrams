//! ADR 0050 E: host-local teardown reconcile — the pure debounce.
//!
//! The coordinator drives every sandbox `destroy` as a best-effort gRPC
//! call; when that call fails (transient gRPC to a loaded/fresh host) the
//! firecracker VM leaks, and nothing reaps it (ADR 0009's reconcile only
//! sweeps the *other* direction, session-alive → sandbox-missing). This
//! module's reconcile (driven in `lib.rs`, which owns the async coord +
//! backend handles) generalizes the migration source-ownership rule
//! (ADR 0045 C1) to all sandboxes: a sandbox whose session no longer owns
//! it (`sessions.sandbox_id` — PG, ADR 0047's sole authority — no longer
//! points at it) is destroyed **locally**, where the destroy can't be
//! defeated by the same gRPC flakiness that leaked it.
//!
//! The one subtlety is *when* to act, and that's the testable part kept
//! here: a sandbox is reaped only after `ORPHAN_STRIKES` CONSECUTIVE
//! orphan verdicts, so an in-flight `create` (whose session binding
//! hasn't been published yet) or a single-tick coordinator blip never
//! reaps a live VM.

use std::collections::HashMap;
use std::time::Duration;

use engram_core::SandboxId;

/// How often the reconcile sweeps. 30s matches the migration TTL sweep
/// cadence; well below the cost of a leaked multi-hundred-MiB VM, well
/// above any create→bind latency.
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Consecutive orphan verdicts required before a local destroy. Two ticks
/// (~60s) clears the create→bind window (a fresh sandbox publishes its
/// session binding within a tick) and rides out a one-tick coord outage.
pub const ORPHAN_STRIKES: u32 = 2;

/// Apply one reconcile tick's verdict for a single sandbox to the strike
/// ledger, returning `true` iff it should be destroyed NOW.
///
/// - `is_orphan == false` (still owned, or coordinator unreachable so we
///   conservatively assume owned) resets the count and returns `false`.
/// - `is_orphan == true` increments the count; once it reaches
///   `threshold`, returns `true` (destroy).
///
/// The caller removes the entry after a successful destroy (and prunes
/// entries for sandboxes that vanished); a destroy that fails leaves the
/// count at/over threshold so the next tick retries immediately.
pub fn orphan_strike(
    strikes: &mut HashMap<SandboxId, u32>,
    sandbox: SandboxId,
    is_orphan: bool,
    threshold: u32,
) -> bool {
    if !is_orphan {
        strikes.remove(&sandbox);
        return false;
    }
    let n = strikes.entry(sandbox).or_insert(0);
    *n += 1;
    *n >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_sandbox_never_strikes() {
        let mut s = HashMap::new();
        let id = SandboxId::new();
        for _ in 0..5 {
            assert!(!orphan_strike(&mut s, id, false, ORPHAN_STRIKES));
        }
        assert!(s.is_empty(), "an owned sandbox leaves no ledger entry");
    }

    #[test]
    fn orphan_destroyed_only_after_consecutive_strikes() {
        let mut s = HashMap::new();
        let id = SandboxId::new();
        // First orphan tick: one strike, below threshold — don't destroy
        // (protects an in-flight create whose binding hasn't landed).
        assert!(!orphan_strike(&mut s, id, true, ORPHAN_STRIKES));
        // Second consecutive orphan tick crosses the threshold.
        assert!(orphan_strike(&mut s, id, true, ORPHAN_STRIKES));
    }

    #[test]
    fn a_single_non_orphan_tick_resets_the_count() {
        let mut s = HashMap::new();
        let id = SandboxId::new();
        assert!(!orphan_strike(&mut s, id, true, ORPHAN_STRIKES)); // strike 1
        assert!(!orphan_strike(&mut s, id, false, ORPHAN_STRIKES)); // owned again → reset
                                                                    // The next orphan run must start the count over, not destroy on its
                                                                    // first strike.
        assert!(!orphan_strike(&mut s, id, true, ORPHAN_STRIKES)); // strike 1 again
        assert!(orphan_strike(&mut s, id, true, ORPHAN_STRIKES)); // strike 2 → destroy
    }

    #[test]
    fn a_failed_destroy_retries_immediately_next_tick() {
        let mut s = HashMap::new();
        let id = SandboxId::new();
        assert!(!orphan_strike(&mut s, id, true, ORPHAN_STRIKES)); // strike 1
        assert!(orphan_strike(&mut s, id, true, ORPHAN_STRIKES)); // strike 2 → destroy attempt
                                                                  // The caller's destroy failed, so it did NOT remove the entry: the
                                                                  // count stays at/over threshold and re-fires on the next tick.
        assert!(orphan_strike(&mut s, id, true, ORPHAN_STRIKES));
    }
}
