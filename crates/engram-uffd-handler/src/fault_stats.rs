//! Fault-path measurement: per-source counts and latency histograms,
//! the populate-lock wait, and the first-touch chunk order.
//!
//! The counters are process-global atomics: one handler process serves
//! one VM. Percentiles are bucket upper bounds, not exact values.

use std::cell::Cell;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Where a served fault got its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Canonical chunk already in the base shm: `UFFDIO_CONTINUE` only.
    Shm = 0,
    /// Read from the local chunk cache.
    Cache = 1,
    /// Populated by the host agent over the substrate socket.
    Populate = 2,
    /// Direct blob fetch (the host agent was unreachable).
    Fallback = 3,
    /// Zero chunk.
    Zero = 4,
    /// Another installer had the chunk; the fault only woke the vCPU.
    Wake = 5,
    /// Served from the migration peer.
    Peer = 6,
}

#[cfg(test)]
const SOURCES: [Source; 7] = [
    Source::Shm,
    Source::Cache,
    Source::Populate,
    Source::Fallback,
    Source::Zero,
    Source::Wake,
    Source::Peer,
];

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Shm => "shm",
            Source::Cache => "cache",
            Source::Populate => "populate",
            Source::Fallback => "fallback",
            Source::Zero => "zero",
            Source::Wake => "wake",
            Source::Peer => "peer",
        }
    }
}

/// Bucket upper bounds in microseconds: 0.1, 0.5, 1, 5, 10, 25, 50,
/// 100, 250, 1000, 5000 ms. The last slot counts everything above.
const BUCKET_US: [u64; 11] = [
    100, 500, 1_000, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 1_000_000, 5_000_000,
];
const BUCKET_LABELS: [&str; 12] = [
    "<=0.1ms", "<=0.5ms", "<=1ms", "<=5ms", "<=10ms", "<=25ms", "<=50ms", "<=100ms", "<=250ms",
    "<=1000ms", "<=5000ms", ">5000ms",
];

pub struct Histogram {
    buckets: [AtomicU64; 12],
    sum_us: AtomicU64,
}

impl Histogram {
    const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; 12],
            sum_us: AtomicU64::new(0),
        }
    }

    pub fn record(&self, elapsed: Duration) {
        let us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        let slot = BUCKET_US
            .iter()
            .position(|&b| us <= b)
            .unwrap_or(BUCKET_US.len());
        self.buckets[slot].fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum()
    }

    /// The upper bound of the bucket that holds quantile `q`.
    fn quantile(&self, counts: &[u64; 12], q: f64) -> &'static str {
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return "-";
        }
        let rank = ((total as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, c) in counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return BUCKET_LABELS[i];
            }
        }
        BUCKET_LABELS[11]
    }

    /// `n=… p50… p90… p99… sum_ms=…`.
    pub fn summary(&self) -> String {
        let counts: [u64; 12] = std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed));
        let n: u64 = counts.iter().sum();
        format!(
            "n={n} p50{} p90{} p99{} sum_ms={}",
            self.quantile(&counts, 0.50),
            self.quantile(&counts, 0.90),
            self.quantile(&counts, 0.99),
            self.sum_us.load(Ordering::Relaxed) / 1000,
        )
    }
}

pub struct FaultStats {
    per_source: [Histogram; 7],
    /// Wait to take the populate client's connection lock, per request
    /// (fault and producer requests both).
    lock_wait: Histogram,
}

impl FaultStats {
    const fn new() -> Self {
        Self {
            per_source: [const { Histogram::new() }; 7],
            lock_wait: Histogram::new(),
        }
    }

    pub fn record_fault(&self, source: Source, elapsed: Duration) {
        self.per_source[source as usize].record(elapsed);
    }

    pub fn record_lock_wait(&self, elapsed: Duration) {
        self.lock_wait.record(elapsed);
    }

    pub fn total_faults(&self) -> u64 {
        self.per_source.iter().map(Histogram::count).sum()
    }

    /// One `fault stats` log line.
    pub fn log(&self, trigger: &'static str) {
        let s = |src: Source| self.per_source[src as usize].summary();
        tracing::info!(
            trigger,
            total = self.total_faults(),
            shm = %s(Source::Shm),
            cache = %s(Source::Cache),
            populate = %s(Source::Populate),
            fallback = %s(Source::Fallback),
            zero = %s(Source::Zero),
            wake = %s(Source::Wake),
            peer = %s(Source::Peer),
            lock_wait_total_ms = self.lock_wait.sum_us.load(Ordering::Relaxed) / 1000,
            lock_wait = %self.lock_wait.summary(),
            "fault stats"
        );
    }
}

pub static STATS: FaultStats = FaultStats::new();

thread_local! {
    /// Set by `ChunkedMemoryBackend::fetch_chunk` on the calling thread
    /// (the future runs on the thread that `block_on`s it).
    static FETCH_SOURCE: Cell<Option<Source>> = const { Cell::new(None) };
}

pub fn set_fetch_source(source: Source) {
    FETCH_SOURCE.with(|c| c.set(Some(source)));
}

pub fn take_fetch_source() -> Option<Source> {
    FETCH_SOURCE.with(Cell::take)
}

/// Every 10 s, log the stats if any fault was served since the last line.
pub fn spawn_periodic_log(on_tick: impl Fn() + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("engram-uffd-stats".to_string())
        .spawn(move || {
            let mut last = u64::MAX;
            loop {
                std::thread::sleep(Duration::from_secs(10));
                on_tick();
                let total = STATS.total_faults();
                if total != last {
                    STATS.log("10s");
                    last = total;
                }
            }
        });
}

/// First-touch chunk indices, one per line, flushed every 256 entries.
pub struct FaultOrder {
    seen: Vec<bool>,
    out: std::io::BufWriter<std::fs::File>,
    unflushed: u32,
}

pub const FAULT_ORDER_FILE: &str = "fault-order.txt";

impl FaultOrder {
    pub fn create(path: &Path, chunk_count: usize) -> std::io::Result<Self> {
        Ok(Self {
            seen: vec![false; chunk_count],
            out: std::io::BufWriter::new(std::fs::File::create(path)?),
            unflushed: 0,
        })
    }

    pub fn record(&mut self, chunk_idx: usize) {
        match self.seen.get_mut(chunk_idx) {
            Some(seen) if !*seen => *seen = true,
            _ => return,
        }
        let _ = writeln!(self.out, "{chunk_idx}");
        self.unflushed += 1;
        if self.unflushed >= 256 {
            self.flush();
        }
    }

    pub fn flush(&mut self) {
        if let Err(e) = self.out.flush() {
            tracing::debug!(error = %e, "fault-order flush failed");
        }
        self.unflushed = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_quantiles_use_bucket_upper_bounds() {
        let h = Histogram::new();
        for _ in 0..90 {
            h.record(Duration::from_micros(50));
        }
        for _ in 0..9 {
            h.record(Duration::from_millis(20));
        }
        h.record(Duration::from_secs(9));
        assert_eq!(h.count(), 100);
        let s = h.summary();
        assert!(
            s.starts_with("n=100 p50<=0.1ms p90<=0.1ms p99<=25ms"),
            "{s}"
        );
    }

    #[test]
    fn empty_histogram_has_no_quantiles() {
        assert_eq!(Histogram::new().summary(), "n=0 p50- p90- p99- sum_ms=0");
    }

    #[test]
    fn fault_order_writes_each_chunk_once_in_first_touch_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FAULT_ORDER_FILE);
        let mut order = FaultOrder::create(&path, 8).unwrap();
        for idx in [5, 2, 5, 7, 2, 0, 99] {
            order.record(idx);
        }
        order.flush();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "5\n2\n7\n0\n");
    }

    #[test]
    fn fetch_source_is_taken_once() {
        set_fetch_source(Source::Populate);
        assert_eq!(take_fetch_source(), Some(Source::Populate));
        assert_eq!(take_fetch_source(), None);
    }

    #[test]
    fn every_source_has_a_slot() {
        for (i, s) in SOURCES.iter().enumerate() {
            assert_eq!(*s as usize, i, "{}", s.name());
        }
    }
}
