//! The operation-boundary crash injector (ADR 0098 P5).
//!
//! [`CrashFs`] wraps [`TokioFs`] and gates every operation. Each call appends
//! its [`FsOp`] to a trace. An armed cut returns an error at the selected
//! operation and at each later operation. The earlier operations run through
//! the production implementation. Durable record and finalize tests derive
//! their crash schedule from this trace.
//!
//! The H5 composition contract still holds: ADR 0099 H5's static tests own
//! byte-level torn states (a truncated file, a flipped bit); this seam owns
//! reachability at OPERATION granularity. `read_dir` sorts its result —
//! sim-only determinism; prod `TokioFs` is untouched.
//!
//! # Storage lies (ADR 0098 Phase 3, R5 — the storage-fault model)
//!
//! P5's cut models process death. R5 also provides a seeded read-corruption
//! arm ([`ReadFault`]). It can flip one byte or substitute selected bytes.
//! The transform runs in the poll that reads the bytes. No I/O timing affects
//! the result. Durable format tests use this arm to check their envelopes.

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

/// How a [`ReadFault`] mangles the bytes a real read returned. Both variants
/// are pure, deterministic transforms applied IN-POLL — no real-I/O timing
/// feeds the corruption, so a seed replays exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadCorruption {
    /// Bit-rot: XOR `0xFF` into the byte at `offset % len` of the bytes read
    /// (a no-op on an empty file). A single flipped byte is the minimal
    /// storage lie — enough to fail a content hash, small enough that a
    /// syntactically-valid JSON corruption often still parses (which is the
    /// whole point: an unchecksummed record TRUSTS it).
    FlipByte { offset: usize },
    /// A stale or MISDIRECTED read: discard the file's real bytes and return
    /// `bytes` instead. The caller supplies an EARLIER version's bytes (a
    /// stale read) or ANOTHER path's bytes (a misdirected read) — the seam is
    /// identical; only the provenance of `bytes` differs.
    Substitute { bytes: Vec<u8> },
}

/// A seeded read-corruption arm: corrupt the [`read`](HostFs::read) op at
/// index `on_read` (0-based over THIS instance's reads; `read_dir` does not
/// count), leaving every other read untouched. Independent of the crash cut —
/// a `CrashFs` is armed with a crash index XOR a read fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadFault {
    pub on_read: usize,
    pub corruption: ReadCorruption,
}

struct CrashFsState {
    trace: Vec<FsOp>,
    crash_at: Option<usize>,
    read_fault: Option<ReadFault>,
    /// Reads served so far — the index a [`ReadFault`] matches on.
    reads_seen: usize,
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
                read_fault: None,
                reads_seen: 0,
            }),
        })
    }

    /// Arm a seeded READ-corruption arm (R5): no crash cut, but the read at
    /// index [`ReadFault::on_read`] returns [corrupted](ReadCorruption) bytes.
    /// Every other op — reads included — runs for real.
    pub fn with_read_fault(fault: ReadFault) -> Arc<Self> {
        Arc::new(Self {
            inner: TokioFs,
            state: Mutex::new(CrashFsState {
                trace: Vec::new(),
                crash_at: None,
                read_fault: Some(fault),
                reads_seen: 0,
            }),
        })
    }

    /// The ops issued so far, in issue order.
    pub fn trace(&self) -> Vec<FsOp> {
        self.state.lock().trace.clone()
    }

    /// If a [`ReadFault`] targets the read at index `read_idx`, apply its
    /// corruption to `bytes` in place. Pure + in-poll: the transform is a
    /// function of the armed fault and the bytes, nothing else.
    fn corrupt_read(&self, read_idx: usize, bytes: &mut Vec<u8>) {
        let st = self.state.lock();
        let Some(fault) = &st.read_fault else {
            return;
        };
        if fault.on_read != read_idx {
            return;
        }
        match &fault.corruption {
            ReadCorruption::FlipByte { offset } => {
                if !bytes.is_empty() {
                    let i = offset % bytes.len();
                    bytes[i] ^= 0xFF;
                }
            }
            ReadCorruption::Substitute { bytes: sub } => {
                *bytes = sub.clone();
            }
        }
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
        // The read index this call occupies (reads only; read_dir excluded).
        let read_idx = {
            let mut st = self.state.lock();
            let idx = st.reads_seen;
            st.reads_seen += 1;
            idx
        };
        let mut bytes = self.inner.read(path).await?;
        // R5: a seeded read fault lies about the stored bytes IN-POLL.
        self.corrupt_read(read_idx, &mut bytes);
        Ok(bytes)
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

#[cfg(test)]
mod tests {
    //! The R5 read-corruption primitive, exercised directly: each mode is a
    //! seeded, in-poll transform of the bytes a real read returned, and it
    //! targets exactly the armed read index (other reads pass through).

    use super::*;

    async fn seed_file(dir: &Path, name: &str, bytes: &[u8]) {
        TokioFs.write(&dir.join(name), bytes).await.unwrap();
    }

    #[tokio::test]
    async fn flip_byte_corrupts_only_the_targeted_read() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        seed_file(dir, "a.bin", b"hello world").await;
        seed_file(dir, "b.bin", b"second file").await;

        // Arm a flip on read #1 (the SECOND read); read #0 is untouched.
        let fs = CrashFs::with_read_fault(ReadFault {
            on_read: 1,
            corruption: ReadCorruption::FlipByte { offset: 2 },
        });
        let first = fs.read(&dir.join("a.bin")).await.unwrap();
        assert_eq!(
            first, b"hello world",
            "read #0 is not the target — untouched"
        );
        let second = fs.read(&dir.join("b.bin")).await.unwrap();
        let mut want = b"second file".to_vec();
        want[2] ^= 0xFF;
        assert_eq!(second, want, "read #1 has exactly one flipped byte");
        // A third read of the same file is not the target either.
        let third = fs.read(&dir.join("b.bin")).await.unwrap();
        assert_eq!(
            third, b"second file",
            "read #2 is past the target — untouched"
        );
    }

    #[tokio::test]
    async fn substitute_serves_stale_or_misdirected_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        seed_file(dir, "current.bin", b"the current version").await;

        // Stale/misdirected: read #0 returns the supplied bytes, not the file's.
        let fs = CrashFs::with_read_fault(ReadFault {
            on_read: 0,
            corruption: ReadCorruption::Substitute {
                bytes: b"an earlier version".to_vec(),
            },
        });
        let got = fs.read(&dir.join("current.bin")).await.unwrap();
        assert_eq!(
            got, b"an earlier version",
            "the file lies about its content"
        );
    }

    #[tokio::test]
    async fn read_dir_does_not_consume_a_read_index() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        seed_file(dir, "only.bin", b"payload").await;

        // read_dir between reads must NOT shift the read index: the fault on
        // read #0 still lands on the first genuine `read`.
        let fs = CrashFs::with_read_fault(ReadFault {
            on_read: 0,
            corruption: ReadCorruption::FlipByte { offset: 0 },
        });
        let _ = fs.read_dir(dir).await.unwrap();
        let got = fs.read(&dir.join("only.bin")).await.unwrap();
        let mut want = b"payload".to_vec();
        want[0] ^= 0xFF;
        assert_eq!(
            got, want,
            "read_dir is not a read — the index still starts at 0"
        );
    }
}
