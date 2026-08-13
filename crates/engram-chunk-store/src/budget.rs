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

/// Default nested cap on BACKGROUND-class PUTs (ADR 0116 C3). Sized
/// well under the global cap so a background workload can never own
/// the whole wire: the 2026-08-12 storm was one checkpoint re-chunk
/// holding all 96 permits at 100-300 PUTs/s while a live NBD serve
/// loop starved into the kernel's 90 s send timeout.
const DEFAULT_BACKGROUND_PERMITS: usize = 24;

/// Env override for the background cap (`0`/unparseable ⇒ default).
const BACKGROUND_PERMITS_ENV: &str = "ENGRAM_UPLOAD_BUDGET_BACKGROUND_PERMITS";

/// ADR 0116 C3: which arbitration class a chunk PUT belongs to.
///
/// `Foreground` = work a live session is waiting on right now: the NBD
/// dirty-tier flush, eviction capture, the shutdown-spool export.
/// `Background` = bulk work nobody is blocked on: the periodic
/// checkpoint re-chunk, materialize/enable chunking, base capture,
/// peer fill. Background draws from a nested cap AND the global cap,
/// so it interleaves with foreground instead of starving it; a solo
/// background workload still gets its full nested width.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum UploadClass {
    #[default]
    Foreground,
    Background,
}

impl UploadClass {
    fn label(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Background => "background",
        }
    }
}

/// Host-wide cap on concurrent chunk-upload PUTs. Cheap to clone;
/// every [`crate::ChunkStore`] on the host should share ONE instance
/// (see `with_upload_budget`).
///
/// **PROCESS-scoped by design** (ADR 0116 C1): the resource this guards
/// — in-flight HTTP PUT bodies on the NIC — cannot outlive the process,
/// so a deploy/restart correctly resets to a full budget (and the
/// in-flight gauge to zero); there is no stranded reservation to
/// recover, unlike the PG leases whose protected work (VMs) outlives
/// its holder. Corollary: "host-wide" holds only while exactly one
/// host-agent process runs per node — true under the OnDelete roll (the
/// old pod fully exits before its successor starts). An overlapping
/// roll strategy would double the effective cap; if that ever changes,
/// this must become node-scoped (C3, which restructures the budget into
/// classes, is the natural home).
#[derive(Clone)]
pub struct UploadBudget {
    permits: Arc<Semaphore>,
    /// ADR 0116 C3: the nested background cap. Background acquisitions
    /// take one of these BEFORE queueing on the global semaphore, so at
    /// most `background` background PUTs ever sit in the global FIFO —
    /// foreground work is never more than that far from the wire.
    background: Arc<Semaphore>,
}

impl UploadBudget {
    pub fn new(permits: usize) -> Self {
        Self::with_background_cap(permits, DEFAULT_BACKGROUND_PERMITS)
    }

    /// A budget with an explicit background cap (tests; `new` and env
    /// wiring pick the default). The cap is clamped to the global width
    /// — a nested cap above the global would be a no-op lie.
    pub fn with_background_cap(permits: usize, background: usize) -> Self {
        let permits = permits.max(1);
        Self {
            permits: Arc::new(Semaphore::new(permits)),
            background: Arc::new(Semaphore::new(background.clamp(1, permits))),
        }
    }

    /// `ENGRAM_UPLOAD_BUDGET_PERMITS` / `_BACKGROUND_PERMITS` or the
    /// defaults (96 / 24).
    pub fn from_env_or_default() -> Self {
        let read = |env: &str, default: usize| {
            std::env::var(env)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|&n| n > 0)
                .unwrap_or(default)
        };
        Self::with_background_cap(
            read(PERMITS_ENV, DEFAULT_UPLOAD_PERMITS),
            read(BACKGROUND_PERMITS_ENV, DEFAULT_BACKGROUND_PERMITS),
        )
    }

    /// Acquire one PUT slot (FIFO). Infallible: the semaphores are never
    /// closed. `Background` first takes a nested-cap slot, then the
    /// global slot, and holds both — two-level acquisition in one fixed
    /// order, so no deadlock and at most the nested cap of background
    /// PUTs in flight OR queued globally. The wait is recorded per class
    /// so budget contention is visible, and the returned guard carries
    /// the per-class in-flight gauge (ADR 0116 C1/C3; `sum()` over the
    /// `class` label is the pre-C3 unlabeled series): a sustained
    /// `background` gauge at its cap alongside idle `foreground` is the
    /// storm ARBITRATED — the 2026-08-12 signature (a checkpoint
    /// re-chunk holding all 96 while a live NBD serve loop starved)
    /// can no longer form.
    pub async fn acquire(&self, class: UploadClass) -> UploadPermit {
        let started = crate::time_source::metrics_now();
        let nested = match class {
            UploadClass::Foreground => None,
            UploadClass::Background => Some(
                Arc::clone(&self.background)
                    .acquire_owned()
                    .await
                    .expect("background budget semaphore never closed"),
            ),
        };
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("upload budget semaphore never closed");
        metrics::histogram!("engram_upload_budget_wait_seconds", "class" => class.label())
            .record(started.elapsed().as_secs_f64());
        metrics::gauge!("engram_upload_budget_in_flight", "class" => class.label()).increment(1.0);
        UploadPermit {
            _permit: permit,
            _nested: nested,
            class,
        }
    }
}

/// RAII PUT slot from [`UploadBudget::acquire`]. Dropping releases the
/// semaphore permit(s) and decrements the in-flight gauge together, so
/// the gauge is exact (never sampled) across every exit path.
pub struct UploadPermit {
    _permit: OwnedSemaphorePermit,
    _nested: Option<OwnedSemaphorePermit>,
    class: UploadClass,
}

impl Drop for UploadPermit {
    fn drop(&mut self) {
        metrics::gauge!("engram_upload_budget_in_flight", "class" => self.class.label())
            .decrement(1.0);
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
                    let _permit = budget.acquire(UploadClass::Foreground).await;
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

    /// ADR 0116 C3: the storm shape, arbitrated. A 32-wide background
    /// workload against a 16-global/4-background budget must never hold
    /// more than 4 slots — leaving the wire open — while a concurrent
    /// foreground workload runs at full remaining width.
    #[tokio::test]
    async fn background_class_is_capped_under_its_nested_limit() {
        let budget = UploadBudget::with_background_cap(16, 4);
        let bg_live = Arc::new(AtomicUsize::new(0));
        let bg_high = Arc::new(AtomicUsize::new(0));
        let fg_high = Arc::new(AtomicUsize::new(0));
        let fg_live = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..32 {
            let budget = budget.clone();
            let live = Arc::clone(&bg_live);
            let high = Arc::clone(&bg_high);
            tasks.push(tokio::spawn(async move {
                let _permit = budget.acquire(UploadClass::Background).await;
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                high.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                live.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for _ in 0..32 {
            let budget = budget.clone();
            let live = Arc::clone(&fg_live);
            let high = Arc::clone(&fg_high);
            tasks.push(tokio::spawn(async move {
                let _permit = budget.acquire(UploadClass::Foreground).await;
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                high.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                live.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let bg = bg_high.load(Ordering::SeqCst);
        let fg = fg_high.load(Ordering::SeqCst);
        assert!(bg <= 4, "background must cap at its nested 4, saw {bg}");
        assert!(
            fg > 4,
            "foreground must run wider than the background cap, saw {fg}"
        );
    }

    /// A SOLO background workload still gets its full nested width —
    /// the cap protects foreground, it does not punish idle systems.
    #[tokio::test]
    async fn solo_background_reaches_its_full_nested_width() {
        let budget = UploadBudget::with_background_cap(16, 4);
        let live = Arc::new(AtomicUsize::new(0));
        let high = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let budget = budget.clone();
            let live = Arc::clone(&live);
            let high = Arc::clone(&high);
            tasks.push(tokio::spawn(async move {
                let _permit = budget.acquire(UploadClass::Background).await;
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                high.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                live.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let high = high.load(Ordering::SeqCst);
        assert!(high > 1, "must overlap, saw {high}");
        assert!(high <= 4, "nested cap holds even solo, saw {high}");
    }

    #[test]
    fn zero_or_garbage_env_falls_back_to_default() {
        // new() clamps to ≥1 regardless; the background cap clamps into
        // [1, global].
        let b = UploadBudget::new(0);
        assert_eq!(b.permits.available_permits(), 1);
        assert_eq!(b.background.available_permits(), 1);
        let b = UploadBudget::with_background_cap(4, 100);
        assert_eq!(
            b.background.available_permits(),
            4,
            "nested cap clamps to the global width"
        );
    }
}
