//! The checkpoint tail-cancellation DECISION (ADR 0098 Phase 2, Flow D
//! rider).
//!
//! `ChainHeadStore` closes the detached-tail cancellation race with a
//! per-sandbox epoch fence: `invalidate` bumps the epoch and unlinks under
//! the io lock BEFORE any FC snapshot create; a cancelled `persist`'s
//! detached tail re-checks the epoch under the same lock immediately
//! before its rename. This module holds that re-check's pure predicate;
//! the RACE itself (a real detached `spawn_blocking` interleaved with an
//! invalidate) stays a host-agent unit test — it needs real threads, which
//! the paused single-thread simulator deliberately does not have.

/// A checkpoint persist tail initiated at `initiated_epoch` may publish
/// (rename into place) only if no invalidate has advanced the sandbox's
/// epoch since — otherwise its baseline was consumed and publishing would
/// resurrect a record describing state the KVM dirty bitmap no longer
/// backs (memory corruption on restore).
pub fn checkpoint_tail_admits_publish(initiated_epoch: u64, current_epoch: u64) -> bool {
    initiated_epoch == current_epoch
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_unbumped_epoch_publishes() {
        assert!(checkpoint_tail_admits_publish(3, 3));
        assert!(!checkpoint_tail_admits_publish(3, 4), "invalidated tail");
        // A stale-future epoch is equally inadmissible — equality, not <=.
        assert!(!checkpoint_tail_admits_publish(4, 3));
    }
}
