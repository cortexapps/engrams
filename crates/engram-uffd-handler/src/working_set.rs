//! Working-set recorder for the UFFD page-fault handler.
//!
//! The recorder is the **producer** side of the
//! [`engram_chunk_store::WorkingSetTrace`] pipeline: as the runtime
//! serves UFFD faults via `UFFDIO_COPY`, it calls
//! [`WorkingSetRecorder::observe`] with the [`ChunkHash`] that backed
//! that fault. Inside a fixed time window the recorder accumulates a
//! deduped, ordered list of chunks; on
//! [`WorkingSetRecorder::finish`] it freezes that list into a
//! `WorkingSetTrace` ready for upload (`traces/<manifest_id>/<host_id>.json`).
//!
//! On the next restore of the same manifest, the runtime reads the
//! published trace and prefaults the listed chunks **before** the
//! vCPUs unfreeze. That replay path is wired in
//! [`crate::runtime`] (Linux-only) and only needs the ordered
//! `Vec<ChunkHash>` — which is why the recorder type lives in this
//! crate (alongside the consumer) but stays target-agnostic so
//! macOS dev can unit-test the bookkeeping.
//!
//! Two facts the recorder bakes in:
//!
//! - **Dedup is intentional.** UFFDIO_COPY on a 512 KiB chunk
//!   installs all 128 pages at once. Subsequent faults inside that
//!   range never reach the handler. But cross-vCPU races *can* race
//!   on the same chunk before the COPY lands, so the recorder
//!   guards with a `HashSet<ChunkHash>` to keep the trace clean.
//! - **Time-bounded.** Once `window` elapses, further observations
//!   are dropped. The window doesn't pause vCPUs — recording is
//!   passive. Five seconds is the canonical default
//!   (matches the FaaSnap / REAP literature).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use engram_chunk_store::manifest::ChunkHash;
use engram_chunk_store::working_set::WorkingSetTrace;

/// In-memory accumulator that the runtime feeds as it serves faults.
/// Single-threaded by design — wrap in `Mutex` at the call site.
pub struct WorkingSetRecorder {
    started_at: Instant,
    window: Duration,
    vcpu_count: u32,
    chunks_in_order: Vec<ChunkHash>,
    seen: HashSet<ChunkHash>,
}

impl WorkingSetRecorder {
    /// `window` of `Duration::ZERO` short-circuits the recorder: it
    /// will never accept an observation. Useful in test fixtures
    /// where the prefault path is the focus, not the recording side.
    pub fn new(vcpu_count: u32, window: Duration) -> Self {
        Self {
            started_at: Instant::now(),
            window,
            vcpu_count,
            chunks_in_order: Vec::new(),
            seen: HashSet::new(),
        }
    }

    /// Whether `observe` will still record. Read by the runtime to
    /// skip the lock + hash work once the window has elapsed.
    pub fn is_open(&self) -> bool {
        self.started_at.elapsed() < self.window
    }

    /// Record this chunk hash if the window is still open AND we
    /// haven't recorded it already in this session. Returns `true`
    /// iff the hash was newly appended.
    pub fn observe(&mut self, hash: ChunkHash) -> bool {
        if !self.is_open() {
            return false;
        }
        if self.seen.insert(hash) {
            self.chunks_in_order.push(hash);
            true
        } else {
            false
        }
    }

    pub fn observed_count(&self) -> usize {
        self.chunks_in_order.len()
    }

    /// ADR 0014 M1.14: snapshot accessors used by the periodic
    /// trace-output dumper. The dumper can't consume `self`
    /// (the fault loop owns it) so we expose copies for read-only
    /// use.
    pub fn chunks_snapshot(&self) -> Vec<ChunkHash> {
        self.chunks_in_order.clone()
    }

    pub fn vcpu_count_snapshot(&self) -> u32 {
        self.vcpu_count
    }

    pub fn window_ms_snapshot(&self) -> u32 {
        u32::try_from(self.window.as_millis()).unwrap_or(u32::MAX)
    }

    /// Freeze the accumulator into a publishable `WorkingSetTrace`.
    /// `capture_window_ms` is taken from the constructor's
    /// `window`, not from wall-clock — the trace describes the
    /// window the runtime *intended* to record over.
    pub fn finish(self) -> WorkingSetTrace {
        let capture_window_ms = u32::try_from(self.window.as_millis()).unwrap_or(u32::MAX);
        let mut trace = WorkingSetTrace::new(self.vcpu_count, capture_window_ms);
        trace.chunks = self.chunks_in_order;
        trace
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(byte: u8) -> ChunkHash {
        ChunkHash::of(&[byte])
    }

    #[test]
    fn open_window_records_in_order() {
        let mut r = WorkingSetRecorder::new(2, Duration::from_secs(5));
        assert!(r.is_open());
        assert!(r.observe(h(1)));
        assert!(r.observe(h(2)));
        assert!(r.observe(h(3)));
        let trace = r.finish();
        assert_eq!(trace.chunks, vec![h(1), h(2), h(3)]);
        assert_eq!(trace.vcpu_count, 2);
        assert_eq!(trace.capture_window_ms, 5000);
    }

    #[test]
    fn dedup_keeps_first_observation_order() {
        let mut r = WorkingSetRecorder::new(1, Duration::from_secs(5));
        assert!(r.observe(h(1)));
        assert!(r.observe(h(2)));
        assert!(!r.observe(h(1)));
        assert!(r.observe(h(3)));
        assert!(!r.observe(h(2)));
        let trace = r.finish();
        assert_eq!(trace.chunks, vec![h(1), h(2), h(3)]);
    }

    #[test]
    fn zero_window_short_circuits() {
        let mut r = WorkingSetRecorder::new(1, Duration::ZERO);
        assert!(!r.is_open());
        assert!(!r.observe(h(1)));
        assert_eq!(r.observed_count(), 0);
        let trace = r.finish();
        assert!(trace.chunks.is_empty());
        assert_eq!(trace.capture_window_ms, 0);
    }

    #[test]
    fn elapsed_window_rejects_new_observations() {
        let mut r = WorkingSetRecorder::new(1, Duration::from_millis(10));
        assert!(r.observe(h(1)));
        std::thread::sleep(Duration::from_millis(30));
        assert!(!r.is_open());
        assert!(!r.observe(h(2)));
        let trace = r.finish();
        assert_eq!(trace.chunks, vec![h(1)]);
    }

    #[test]
    fn finish_uses_constructor_window_not_wall_clock() {
        let r = WorkingSetRecorder::new(4, Duration::from_secs(7));
        let trace = r.finish();
        assert_eq!(trace.capture_window_ms, 7000);
        assert_eq!(trace.vcpu_count, 4);
    }

    #[test]
    fn observed_count_tracks_deduped_inserts() {
        let mut r = WorkingSetRecorder::new(1, Duration::from_secs(5));
        r.observe(h(1));
        r.observe(h(1));
        r.observe(h(2));
        assert_eq!(r.observed_count(), 2);
    }
}
