//! Concurrency plumbing for the parallel flatten (ADR 0088 addendum).
//!
//! Two pieces, both deliberately dumb:
//!
//! - [`WritePool`] — a bounded pool of worker threads that receive
//!   **already-open file descriptors** and do only fd-scoped work
//!   (write → fchmod → fsetxattr → futimens). Workers never touch
//!   paths, so every namespace operation (create/unlink/whiteout/
//!   readdir) stays with the single reader thread in exact tar order —
//!   the property that makes the parallel flatten bit-identical to the
//!   sequential one.
//! - [`ChannelReader`] — moves a decompressor onto its own thread and
//!   exposes the output as a plain [`Read`], so inflate overlaps the
//!   reader's syscall work instead of serializing with it.

use std::io::Read;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};

use super::SkippedXattr;

/// Entries larger than this are written inline by the reader
/// (streaming, constant memory); everything else is buffered and
/// dispatched to the pool. 16 MiB keeps the pool's byte gate
/// meaningful while the giant-blob case (model weights, jars) stays
/// bandwidth-bound on the reader where it already was.
pub(super) const INLINE_WRITE_THRESHOLD: u64 = 16 * 1024 * 1024;

/// Cap on buffered job bytes in flight across the pool — bounds the
/// flatten's memory to ~this plus one inline entry, whatever the
/// entry-size distribution.
const MAX_INFLIGHT_BYTES: u64 = 384 * 1024 * 1024;

/// Bounded job-count backstop (tiny files: the byte gate alone would
/// admit millions of empty entries).
const MAX_INFLIGHT_JOBS: usize = 4096;

/// Worker count: 2× the cgroup-aware core count, clamped to [4, 16].
/// The workload is small-file syscall latency, not CPU — a few more
/// threads than cores helps; `ENGRAM_FLATTEN_WRITE_CONCURRENCY`
/// overrides for prod tuning.
pub fn default_write_concurrency() -> usize {
    if let Ok(v) = std::env::var("ENGRAM_FLATTEN_WRITE_CONCURRENCY") {
        if let Ok(n) = v.parse::<usize>() {
            return n.clamp(1, 64);
        }
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (2 * cores).clamp(4, 16)
}

/// One dispatched regular-file write. The reader created (and thereby
/// named) the file; the worker owns only this fd.
pub(super) struct WriteJob {
    pub file: std::fs::File,
    /// Tree-relative path — error/xattr reporting only, never opened.
    pub rel: String,
    pub bytes: Vec<u8>,
    pub mode: u32,
    pub mtime: u64,
    pub xattrs: Vec<(String, Vec<u8>)>,
}

struct PoolShared {
    /// First worker error wins; the reader checks between entries and
    /// `finish()` surfaces it.
    failure: Mutex<Option<(String, std::io::Error)>>,
    /// Fd-applied xattr refusals, drained into `TreeMetadata` at
    /// `finish()` (same "collected, never silently lost" contract as
    /// the sequential path).
    skipped_xattrs: Mutex<Vec<SkippedXattr>>,
    inflight: Mutex<Inflight>,
    freed: Condvar,
}

#[derive(Default)]
struct Inflight {
    bytes: u64,
    jobs: usize,
}

/// Bounded fd-scoped write pool. See the module docs for the safety
/// argument; the short version is that a worker cannot observe or
/// perturb the namespace, only the inode it was handed.
pub(super) struct WritePool {
    tx: Option<SyncSender<WriteJob>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    shared: Arc<PoolShared>,
}

impl WritePool {
    pub fn new(concurrency: usize) -> std::io::Result<Self> {
        let concurrency = concurrency.max(1);
        // The channel bound is a handoff buffer, not the real gate —
        // the byte/job gate in `submit` is what bounds memory.
        let (tx, rx) = std::sync::mpsc::sync_channel::<WriteJob>(concurrency * 2);
        let rx = Arc::new(Mutex::new(rx));
        let shared = Arc::new(PoolShared {
            failure: Mutex::new(None),
            skipped_xattrs: Mutex::new(Vec::new()),
            inflight: Mutex::new(Inflight::default()),
            freed: Condvar::new(),
        });
        let mut workers = Vec::with_capacity(concurrency);
        for i in 0..concurrency {
            let rx = Arc::clone(&rx);
            let shared = Arc::clone(&shared);
            workers.push(
                std::thread::Builder::new()
                    .name(format!("flatten-write-{i}"))
                    .spawn(move || worker_loop(&rx, &shared))?,
            );
        }
        Ok(Self {
            tx: Some(tx),
            workers,
            shared,
        })
    }

    /// True once any worker has failed — the reader polls this between
    /// entries to abort early instead of queueing more work.
    pub fn failed(&self) -> bool {
        self.shared.failure.lock().unwrap().is_some()
    }

    /// Hand a job to the pool, blocking on the byte/job gate.
    pub fn submit(&self, job: WriteJob) -> std::io::Result<()> {
        let cost = gate_cost(job.bytes.len() as u64);
        {
            let mut inflight = self.shared.inflight.lock().unwrap();
            while inflight.bytes + cost > MAX_INFLIGHT_BYTES || inflight.jobs >= MAX_INFLIGHT_JOBS {
                // A failed pool drains fast (workers release as they
                // drop jobs), so this wait cannot deadlock on failure.
                if self.shared.failure.lock().unwrap().is_some() {
                    break;
                }
                inflight = self.shared.freed.wait(inflight).unwrap();
            }
            inflight.bytes += cost;
            inflight.jobs += 1;
        }
        let tx = self.tx.as_ref().expect("submit after finish");
        // Blocking send: the channel bound is a small handoff buffer;
        // backpressure is intentional. A disconnected channel means
        // every worker exited (only possible via panic) — surface as
        // an io error rather than panicking the reader.
        tx.send(job).map_err(|e| {
            // Release the gate units the lost job was holding.
            let cost = gate_cost(e.0.bytes.len() as u64);
            let mut inflight = self.shared.inflight.lock().unwrap();
            inflight.bytes = inflight.bytes.saturating_sub(cost);
            inflight.jobs = inflight.jobs.saturating_sub(1);
            std::io::Error::other("flatten write pool disconnected")
        })
    }

    /// Join every worker and surface the first failure. MUST run (and
    /// return `Ok`) before anything consumes the tree — ownership pass,
    /// mtime clamp, pack.
    pub fn finish(mut self) -> Result<Vec<SkippedXattr>, (String, std::io::Error)> {
        drop(self.tx.take()); // close the channel: workers drain + exit
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        if let Some(fail) = self.shared.failure.lock().unwrap().take() {
            return Err(fail);
        }
        Ok(std::mem::take(
            &mut self.shared.skipped_xattrs.lock().unwrap(),
        ))
    }
}

/// Empty files still cost one gate unit so `MAX_INFLIGHT_JOBS` is the
/// binding constraint for tiny-file storms, not the byte gate.
fn gate_cost(len: u64) -> u64 {
    len.max(1)
}

fn worker_loop(rx: &Mutex<Receiver<WriteJob>>, shared: &PoolShared) {
    loop {
        let job = {
            let rx = rx.lock().unwrap();
            match rx.recv() {
                Ok(j) => j,
                Err(_) => return, // channel closed: finish() is joining
            }
        };
        let cost = gate_cost(job.bytes.len() as u64);
        let already_failed = shared.failure.lock().unwrap().is_some();
        if !already_failed {
            match run_job(&job) {
                Ok(mut skipped) => {
                    if !skipped.is_empty() {
                        shared.skipped_xattrs.lock().unwrap().append(&mut skipped);
                    }
                }
                Err(e) => {
                    let mut slot = shared.failure.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some((job.rel.clone(), e));
                    }
                }
            }
        }
        // Release the gate whether the job ran, failed, or was dropped
        // on an already-failed pool — submitters must never wedge.
        {
            let mut inflight = shared.inflight.lock().unwrap();
            inflight.bytes = inflight.bytes.saturating_sub(cost);
            inflight.jobs = inflight.jobs.saturating_sub(1);
        }
        shared.freed.notify_all();
    }
}

/// The fd-scoped half of a regular-file extraction. Order matters:
/// content first, then mode (fchmod — may drop our own write
/// permission, e.g. mode 0444), then xattrs, then futimens LAST so
/// the writes don't bump the deterministic mtime.
fn run_job(job: &WriteJob) -> std::io::Result<Vec<SkippedXattr>> {
    use std::io::Write;
    let mut f = &job.file;
    f.write_all(&job.bytes)?;
    finish_file_fd(&job.file, &job.rel, job.mode, job.mtime, &job.xattrs)
}

/// fd-scoped attribute application shared by workers and the reader's
/// inline large-entry path: fchmod → fsetxattr (refusals collected,
/// never fatal) → futimens last.
pub(super) fn finish_file_fd(
    file: &std::fs::File,
    rel: &str,
    mode: u32,
    mtime: u64,
    xattrs: &[(String, Vec<u8>)],
) -> std::io::Result<Vec<SkippedXattr>> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    let mut skipped = Vec::new();
    for (name, value) in xattrs {
        if let Err(e) = xattr::FileExt::set_xattr(file, name, value) {
            skipped.push(SkippedXattr {
                path: rel.to_string(),
                name: name.clone(),
                error: e.to_string(),
            });
        }
    }
    let ft = filetime::FileTime::from_unix_time(mtime as i64, 0);
    filetime::set_file_handle_times(file, Some(ft), Some(ft))?;
    Ok(skipped)
}

/// Runs a `Read` (in practice: a decompressor) on its own thread and
/// re-exposes it as a `Read` of ~1 MiB blocks — inflate no longer
/// serializes with the flatten's syscalls. Dropping the reader early
/// disconnects the channel and the thread exits on its next send.
pub struct ChannelReader {
    rx: Receiver<std::io::Result<Vec<u8>>>,
    cur: Vec<u8>,
    pos: usize,
    done: bool,
    _thread: std::thread::JoinHandle<()>,
}

impl ChannelReader {
    const BLOCK: usize = 1024 * 1024;
    const DEPTH: usize = 8;

    pub fn spawn<R: Read + Send + 'static>(mut src: R) -> std::io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<std::io::Result<Vec<u8>>>(Self::DEPTH);
        let thread = std::thread::Builder::new()
            .name("flatten-inflate".into())
            .spawn(move || loop {
                let mut buf = vec![0u8; Self::BLOCK];
                match src.read(&mut buf) {
                    Ok(0) => return, // EOF: drop tx, reader sees Ok(0)
                    Ok(n) => {
                        buf.truncate(n);
                        if tx.send(Ok(buf)).is_err() {
                            return; // consumer gone (early drop / error)
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                }
            })?;
        Ok(Self {
            rx,
            cur: Vec::new(),
            pos: 0,
            done: false,
            _thread: thread,
        })
    }
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.pos < self.cur.len() {
                let n = out.len().min(self.cur.len() - self.pos);
                out[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if self.done {
                return Ok(0);
            }
            match self.rx.recv() {
                Ok(Ok(block)) => {
                    self.cur = block;
                    self.pos = 0;
                }
                Ok(Err(e)) => {
                    self.done = true;
                    return Err(e);
                }
                Err(_) => {
                    self.done = true; // producer finished (EOF)
                }
            }
        }
    }
}
