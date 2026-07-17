//! The op-boundary crash injector (ADR 0098 P5) — the REAL `HostFs`
//! interception that replaces P2–P4's externally-constructed post-crash
//! states.
//!
//! [`CrashFs`] wraps the production [`TokioFs`] and gates every operation:
//! each call appends its [`FsOp`] to a shared trace, and when a crash index
//! is armed, the op at (and after) that index returns an error WITHOUT
//! performing — so ops `0..k` ran for real and the on-disk state is exactly
//! what a process death before op `k` leaves. Because the shipped
//! `durable_record::persist` / `spool::write_spool` bodies themselves issue
//! the ops through the seam, the crash-point list is **derived from the
//! production op sequence by running it** — never a hand-maintained parallel
//! table (`tests/crashpoint_coverage.rs` pins the derivation).
//!
//! The H5 composition contract still holds: ADR 0099 H5's static tests own
//! byte-level torn states (a truncated file, a flipped bit); this seam owns
//! reachability at OPERATION granularity. `read_dir` sorts its result —
//! sim-only determinism; prod `TokioFs` is untouched.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use engram_host_core::{HostFs, TokioFs};
use parking_lot::Mutex;

/// One `HostFs` operation kind, as the flows issue them. Wildcard-free at
/// its use sites; a new `HostFs` method must add a variant here or the
/// [`CrashFs`] impl fails to compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsOp {
    Write,
    SyncFile,
    Rename,
    SyncDir,
    Read,
    ReadDir,
    RemoveFile,
    CreateDir,
    RemoveDir,
}

struct CrashFsState {
    trace: Vec<FsOp>,
    crash_at: Option<usize>,
}

/// A [`HostFs`] that records every op and, when armed, refuses the op at
/// index `crash_at` and everything after it. Ops before the cut delegate to
/// the real [`TokioFs`], so the surviving on-disk state is genuine.
pub struct CrashFs {
    inner: TokioFs,
    state: Mutex<CrashFsState>,
}

impl CrashFs {
    /// Recording-only: no crash armed. Used to DERIVE the production op
    /// trace of a flow (the coverage meta-test) and as an un-cut baseline.
    pub fn recording() -> Arc<Self> {
        Self::with_crash_at(None)
    }

    /// Arm a cut at op index `k` (0-based over this instance's whole
    /// lifetime): op `k` and every later op fail without performing.
    /// `None` = never cut.
    pub fn with_crash_at(k: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            inner: TokioFs,
            state: Mutex::new(CrashFsState {
                trace: Vec::new(),
                crash_at: k,
            }),
        })
    }

    /// The ops issued so far, in issue order.
    pub fn trace(&self) -> Vec<FsOp> {
        self.state.lock().trace.clone()
    }

    /// Record `op`; error if the cut index is reached. The error kind is
    /// `Other` — the flows treat any io error as a failed durable op, which
    /// is exactly what a mid-flow process death looks like to a redrive.
    fn gate(&self, op: FsOp) -> io::Result<()> {
        let mut st = self.state.lock();
        let idx = st.trace.len();
        st.trace.push(op);
        if st.crash_at.is_some_and(|k| idx >= k) {
            return Err(io::Error::other(format!(
                "injected crash: fs op {idx} ({op:?}) cut"
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl HostFs for CrashFs {
    async fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.gate(FsOp::Write)?;
        self.inner.write(path, bytes).await
    }

    async fn sync_file(&self, path: &Path) -> io::Result<()> {
        self.gate(FsOp::SyncFile)?;
        self.inner.sync_file(path).await
    }

    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.gate(FsOp::Rename)?;
        self.inner.rename(from, to).await
    }

    async fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.gate(FsOp::SyncDir)?;
        self.inner.sync_dir(dir).await
    }

    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.gate(FsOp::Read)?;
        self.inner.read(path).await
    }

    async fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        self.gate(FsOp::ReadDir)?;
        let mut entries = self.inner.read_dir(dir).await?;
        // Determinism: the OS readdir order must never feed a sim decision.
        entries.sort();
        Ok(entries)
    }

    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.gate(FsOp::RemoveFile)?;
        self.inner.remove_file(path).await
    }

    async fn create_dir(&self, dir: &Path) -> io::Result<()> {
        self.gate(FsOp::CreateDir)?;
        self.inner.create_dir(dir).await
    }

    async fn remove_dir(&self, dir: &Path) -> io::Result<()> {
        self.gate(FsOp::RemoveDir)?;
        self.inner.remove_dir(dir).await
    }
}
