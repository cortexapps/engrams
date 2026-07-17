//! The eviction-finalize DECISIONS (ADR 0098 Phase 2, Flow D).
//!
//! `snapshot_begin` moves a paused sandbox's durability boundary from
//! "upload complete" to "record persisted": the drained disk chunks, the FC
//! memory snapshot, and the aux-bundle pins are staged locally and a
//! durable `EvictionFinalizeRecord` is written; a background job then walks
//! the legs (disk manifest publish → memory chunking → blob upload →
//! terminal checkpoint record + destroy), persisting the stage bump after
//! each leg BEFORE deleting the leg's consumed input, so a crash re-enters
//! at the persisted stage.
//!
//! This module holds the **pure decisions** of that flow, extracted out of
//! the host-agent driver so the host-internal simulator (`engram-dst-host`)
//! drives the real leg bodies on macOS and asserts stage monotonicity +
//! convergence. Only `std` types — the portable crate is the honest home
//! (the same test as Flow A's `shutdown` module).
//!
//! **What stays in the driver:** the effectful leg bodies (chunk-store
//! puts, blob uploads, fs), the redrive loop's `sleep`, and the sandbox
//! capture lock the job holds for its lifetime.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Stage-explicit progress marker for one eviction finalize. Each variant
/// means "everything up to and including this leg is durable";
/// `run_eviction_finalize_once` skips legs already past the persisted
/// stage. Declaration order **is** the progression order (the derived
/// [`Ord`] compares by it): `Captured < DiskUploaded < MemoryChunked <
/// BlobsUploaded`. The terminal transition is the record's deletion, not a
/// variant. Serde variant names are load-bearing: they are the on-disk
/// `EvictionFinalizeRecord.stage` encoding.
#[derive(
    Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
pub enum FinalizeStage {
    /// The staging dir + record are durable; no leg has run.
    #[default]
    Captured,
    /// The disk manifest (drained NBD chunks layered onto the capture-time
    /// base) is published to the chunk store.
    DiskUploaded,
    /// The memory snapshot is chunked + its manifest published.
    MemoryChunked,
    /// `state.bin` / `manifest.json` / aux bundles are uploaded.
    BlobsUploaded,
}

impl FinalizeStage {
    /// Every stage, in progression order. A new [`FinalizeStage`] variant
    /// that is not added here is a compile error at the array literal.
    pub const LADDER: [FinalizeStage; 4] = [
        FinalizeStage::Captured,
        FinalizeStage::DiskUploaded,
        FinalizeStage::MemoryChunked,
        FinalizeStage::BlobsUploaded,
    ];

    /// The next stage in the linear ladder, or `None` at the terminal
    /// [`BlobsUploaded`](FinalizeStage::BlobsUploaded) (whose own "next"
    /// is the record's deletion).
    pub fn next(self) -> Option<FinalizeStage> {
        let i = Self::LADDER.iter().position(|s| *s == self)?;
        Self::LADDER.get(i + 1).copied()
    }

    /// A stage transition is legal only one rung forward — the legs bump
    /// exactly one stage each, and a redrive re-enters at the persisted
    /// stage rather than skipping ahead.
    pub fn can_advance_to(self, to: FinalizeStage) -> bool {
        self.next() == Some(to)
    }
}

/// Backoff unit for the redrive loop: `BACKOFF_UNIT * attempt`, capped at
/// [`BACKOFF_CAP`].
pub const BACKOFF_UNIT: Duration = Duration::from_secs(30);
/// Cap on the redrive backoff.
pub const BACKOFF_CAP: Duration = Duration::from_secs(300);

/// The redrive verdict after a failed finalize pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalizeRetry {
    /// Try again after `backoff`.
    Retry { backoff: Duration },
    /// Attempts exhausted: quarantine the record (`finalize/failed/`) and
    /// stop — the honest durability floor falls back to the prior periodic
    /// checkpoint. Guarantees every redrive loop terminates (the sim's
    /// convergence oracle leans on this).
    Quarantine,
}

/// Decide what a failed pass does next. `attempts` is the count AFTER the
/// failed pass was recorded.
pub fn plan_finalize_retry(attempts: u32, max_attempts: u32) -> FinalizeRetry {
    if attempts >= max_attempts {
        return FinalizeRetry::Quarantine;
    }
    FinalizeRetry::Retry {
        backoff: (BACKOFF_UNIT * attempts).min(BACKOFF_CAP),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_is_linear_and_ordered() {
        // Declaration order == Ord order == LADDER order.
        for pair in FinalizeStage::LADDER.windows(2) {
            assert!(pair[0] < pair[1]);
            assert_eq!(pair[0].next(), Some(pair[1]));
            assert!(pair[0].can_advance_to(pair[1]));
            assert!(!pair[1].can_advance_to(pair[0]));
        }
        assert_eq!(FinalizeStage::BlobsUploaded.next(), None);
        assert_eq!(FinalizeStage::default(), FinalizeStage::Captured);
        // Skipping a rung is never a legal advance.
        assert!(!FinalizeStage::Captured.can_advance_to(FinalizeStage::MemoryChunked));
    }

    #[test]
    fn serde_names_are_the_on_disk_encoding() {
        // The variant names are EvictionFinalizeRecord's persisted stage
        // encoding — a rename is an on-disk format break, so pin them.
        for (stage, name) in [
            (FinalizeStage::Captured, "\"Captured\""),
            (FinalizeStage::DiskUploaded, "\"DiskUploaded\""),
            (FinalizeStage::MemoryChunked, "\"MemoryChunked\""),
            (FinalizeStage::BlobsUploaded, "\"BlobsUploaded\""),
        ] {
            assert_eq!(serde_json::to_string(&stage).unwrap(), name);
        }
    }

    #[test]
    fn retry_ladder_backs_off_then_quarantines() {
        assert_eq!(
            plan_finalize_retry(1, 10),
            FinalizeRetry::Retry {
                backoff: Duration::from_secs(30)
            },
        );
        assert_eq!(
            plan_finalize_retry(9, 10),
            FinalizeRetry::Retry {
                backoff: Duration::from_secs(270)
            },
        );
        assert_eq!(plan_finalize_retry(10, 10), FinalizeRetry::Quarantine);
        assert_eq!(plan_finalize_retry(11, 10), FinalizeRetry::Quarantine);
        // The cap binds once attempt*unit exceeds it.
        assert_eq!(
            plan_finalize_retry(20, 30),
            FinalizeRetry::Retry {
                backoff: BACKOFF_CAP
            },
        );
        // A zero max quarantines immediately — no infinite loop config.
        assert_eq!(plan_finalize_retry(0, 0), FinalizeRetry::Quarantine);
    }
}
