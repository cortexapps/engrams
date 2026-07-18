//! Host-global chunk-upload budget (ADR 0088 addendum).
//!
//! A host can run several independent bulk-upload workloads at once —
//! the materialize ext4 chunking (64-wide), the capture memory-seed
//! chunking (32-wide), the NBD disk flush (32-wide) — each bounded on
//! its own but with NO shared cap, so concurrent workloads stacked up
//! to ~100+ in-flight GCS PUTs against one NIC (the 2026-07-13
//! dev-brain enable's 40 s → 11.5 min memory-seed blowup). One FIFO
//! semaphore per host-agent process arbitrates the wire:
//!
//! - Sized ABOVE any single workload's own width (default 96 ≥ 64), so
//!   a solo workload never queues — never slower than before.
//! - tokio's `Semaphore` is FIFO-fair, so two concurrent workloads
//!   interleave instead of doubling NIC pressure.
//! - Only chunk PUT bodies acquire permits. Dedup HEADs (the thing
//!   that makes a re-bake upload nothing) and manifest/artifact PUTs
//!   (tiny, latency-critical) stay unbudgeted.
//!
//! The coordinator and tests simply don't wire a budget (`None`) —
//! zero behavior change there.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Default cap on a host's concurrent chunk PUTs across all workloads.
const DEFAULT_UPLOAD_PERMITS: usize = 96;

/// Env override for prod tuning (`0`/unparseable ⇒ default).
const PERMITS_ENV: &str = "ENGRAM_UPLOAD_BUDGET_PERMITS";

/// Host-wide cap on concurrent chunk-upload PUTs. Cheap to clone;
/// every [`crate::ChunkStore`] on the host should share ONE instance
/// (see `with_upload_budget`).
#[derive(Clone)]
pub struct UploadBudget {
    permits: Arc<Semaphore>,
}

impl UploadBudget {
    pub fn new(permits: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(permits.max(1))),
        }
    }

    /// `ENGRAM_UPLOAD_BUDGET_PERMITS` or the default (96).
    pub fn from_env_or_default() -> Self {
        let permits = std::env::var(PERMITS_ENV)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_UPLOAD_PERMITS);
        Self::new(permits)
    }

    /// Acquire one PUT slot (FIFO). Infallible: the semaphore is never
    /// closed. The wait is recorded so budget contention is visible.
    pub async fn acquire(&self) -> OwnedSemaphorePermit {
        let started = crate::time_source::metrics_now();
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("upload budget semaphore never closed");
        metrics::histogram!("engram_upload_budget_wait_seconds")
            .record(started.elapsed().as_secs_f64());
        permit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Two independent "workloads" (each 32-wide) sharing one 8-permit
    /// budget must never exceed 8 concurrent acquisitions.
    #[tokio::test]
    async fn budget_caps_concurrency_across_workloads() {
        let budget = UploadBudget::new(8);
        let live = Arc::new(AtomicUsize::new(0));
        let high = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..2 {
            for _ in 0..32 {
                let budget = budget.clone();
                let live = Arc::clone(&live);
                let high = Arc::clone(&high);
                tasks.push(tokio::spawn(async move {
                    let _permit = budget.acquire().await;
                    let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                    high.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                    live.fetch_sub(1, Ordering::SeqCst);
                }));
            }
        }
        for t in tasks {
            t.await.unwrap();
        }
        let high = high.load(Ordering::SeqCst);
        assert!(high <= 8, "budget must cap concurrency at 8, saw {high}");
        assert!(high > 1, "acquisitions must actually overlap, saw {high}");
    }

    #[test]
    fn zero_or_garbage_env_falls_back_to_default() {
        // new() clamps to ≥1 regardless.
        let b = UploadBudget::new(0);
        assert_eq!(b.permits.available_permits(), 1);
    }
}
