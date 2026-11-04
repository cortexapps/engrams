//! The op-boundary crash injector for the session-disk seam.
//!
//! [`CrashDisk`] wraps a real [`FileDisk`] and gates every
//! [`SessionDisk`] call the way [`CrashFs`](crate::CrashFs) gates
//! `HostFs`: each call appends its [`DiskOp`] to a trace, and when a
//! crash index is armed, the op at (and after) that index returns an
//! error without performing. Ops before the cut run for real, so the
//! volumes left in the directory are exactly what a process death at
//! that point leaves. A flow's crash-point list is derived by running
//! it once in recording mode and reading the trace.
//!
//! Chunk reads and writes on a [`Volume`] are not gated: they go to
//! the kernel directly in production too, and a death between two of
//! them is a torn volume, which is the checkpoint snapshot's job to
//! exclude, not this injector's.

use std::io;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use engram_host_core::{Capacity, ChunkIndex, FileDisk, SessionDisk, Volume, VolumeName};
use parking_lot::Mutex;

/// One [`SessionDisk`] operation kind. Wildcard-free at its use sites:
/// a new trait method must add a variant here or the impl below fails
/// to compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskOp {
    Create,
    Snapshot,
    Open,
    Delete,
    Capacity,
    DirtyChunks,
}

struct CrashDiskState {
    trace: Vec<DiskOp>,
    crash_at: Option<usize>,
}

/// A [`SessionDisk`] over real files that records every op and, when
/// armed, refuses the op at index `crash_at` and everything after it.
pub struct CrashDisk {
    inner: FileDisk,
    state: Mutex<CrashDiskState>,
}

impl CrashDisk {
    /// Recording-only: no crash armed.
    pub fn recording(root: &Path, chunk_size: u64) -> io::Result<Arc<Self>> {
        Self::with_crash_at(root, chunk_size, None)
    }

    /// Arm a cut at op index `k` (0-based over this instance's whole
    /// lifetime): op `k` and every later op fail without performing.
    /// `None` never cuts.
    pub fn with_crash_at(root: &Path, chunk_size: u64, k: Option<usize>) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            inner: FileDisk::new(root, chunk_size)?,
            state: Mutex::new(CrashDiskState {
                trace: Vec::new(),
                crash_at: k,
            }),
        }))
    }

    /// Every op issued so far, in order, including the refused ones.
    pub fn trace(&self) -> Vec<DiskOp> {
        self.state.lock().trace.clone()
    }

    /// The real disk underneath, for a test to inspect what survived.
    pub fn inner(&self) -> &FileDisk {
        &self.inner
    }

    fn gate(&self, op: DiskOp) -> io::Result<()> {
        let mut st = self.state.lock();
        let idx = st.trace.len();
        st.trace.push(op);
        if st.crash_at.is_some_and(|k| idx >= k) {
            return Err(io::Error::other(format!(
                "injected crash: disk op {idx} ({op:?}) cut"
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl SessionDisk for CrashDisk {
    async fn create(&self, name: &VolumeName, size_bytes: u64) -> io::Result<Volume> {
        self.gate(DiskOp::Create)?;
        self.inner.create(name, size_bytes).await
    }

    async fn snapshot(&self, origin: &Volume, name: &VolumeName) -> io::Result<Volume> {
        self.gate(DiskOp::Snapshot)?;
        self.inner.snapshot(origin, name).await
    }

    async fn open(&self, name: &VolumeName) -> io::Result<Volume> {
        self.gate(DiskOp::Open)?;
        self.inner.open(name).await
    }

    async fn delete(&self, name: &VolumeName) -> io::Result<()> {
        self.gate(DiskOp::Delete)?;
        self.inner.delete(name).await
    }

    async fn capacity(&self) -> io::Result<Capacity> {
        self.gate(DiskOp::Capacity)?;
        self.inner.capacity().await
    }

    async fn dirty_chunks(&self, a: &Volume, b: &Volume) -> io::Result<Vec<ChunkIndex>> {
        self.gate(DiskOp::DirtyChunks)?;
        self.inner.dirty_chunks(a, b).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: u64 = 4096;

    fn name(s: &str) -> VolumeName {
        VolumeName::new(s).expect("valid name")
    }

    /// A small flow: create, snapshot, delete the origin. Run it once to
    /// derive its op trace, then cut at every index and check the files
    /// left behind match a death at that op.
    async fn flow(disk: &dyn SessionDisk) -> io::Result<()> {
        let origin = disk.create(&name("origin"), CHUNK).await?;
        disk.snapshot(&origin, &name("snap")).await?;
        disk.delete(&name("origin")).await
    }

    #[tokio::test]
    async fn recording_derives_the_op_trace() {
        let dir = tempfile::tempdir().unwrap();
        let disk = CrashDisk::recording(dir.path(), CHUNK).unwrap();
        flow(disk.as_ref()).await.unwrap();
        assert_eq!(
            disk.trace(),
            vec![DiskOp::Create, DiskOp::Snapshot, DiskOp::Delete]
        );
    }

    #[tokio::test]
    async fn a_cut_leaves_exactly_the_state_of_the_ops_before_it() {
        for k in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let disk = CrashDisk::with_crash_at(dir.path(), CHUNK, Some(k)).unwrap();
            let err = flow(disk.as_ref()).await.unwrap_err();
            assert!(err.to_string().contains("injected crash"), "k={k}: {err}");

            let exists = |n: &str| dir.path().join(n).exists();
            // k=0: nothing ran. k=1: origin created. k=2: origin + snapshot.
            assert_eq!(exists("origin"), k >= 1, "k={k}");
            assert_eq!(exists("snap"), k >= 2, "k={k}");
            // The refused op is still in the trace, nothing after it.
            assert_eq!(disk.trace().len(), k + 1, "k={k}");
        }
    }
}
