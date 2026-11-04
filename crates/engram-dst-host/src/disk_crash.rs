//! [`CrashDisk`]: a real [`FileDisk`] with every [`SessionDisk`] call
//! traced and, when armed, cut at an op index, like [`CrashFs`](crate::CrashFs).
//! Chunk reads and writes are not gated: they go to the kernel in
//! production too.

use std::io;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use engram_host_core::{Capacity, ChunkIndex, FileDisk, SessionDisk, Volume, VolumeName};
use parking_lot::Mutex;

/// One [`SessionDisk`] operation kind.
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

pub struct CrashDisk {
    inner: FileDisk,
    state: Mutex<CrashDiskState>,
}

impl CrashDisk {
    pub fn recording(root: &Path, chunk_size: u64) -> io::Result<Arc<Self>> {
        Self::with_crash_at(root, chunk_size, None)
    }

    /// Op `k` and every later op fail without performing.
    pub fn with_crash_at(root: &Path, chunk_size: u64, k: Option<usize>) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            inner: FileDisk::new(root, chunk_size)?,
            state: Mutex::new(CrashDiskState {
                trace: Vec::new(),
                crash_at: k,
            }),
        }))
    }

    /// Every op issued so far, including refused ones.
    pub fn trace(&self) -> Vec<DiskOp> {
        self.state.lock().trace.clone()
    }

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
    use std::sync::Arc;

    use engram_chunk_store::{ChunkStore, Manifest};
    use engram_host_core::{checkpoint, stage};
    use engram_storage_local::LocalBlobStorage;

    use super::*;

    const CHUNK: u64 = 4096;

    fn store(root: &Path) -> ChunkStore {
        ChunkStore::new(Arc::new(LocalBlobStorage::new(root.join("blobs"))))
    }

    fn fill(byte: u8) -> Vec<u8> {
        vec![byte; CHUNK as usize]
    }

    /// Two checkpoints of one live volume with writes in between, then
    /// the first snapshot deleted. Returns the second manifest.
    async fn checkpoint_flow(disk: &dyn SessionDisk, store: &ChunkStore) -> io::Result<Manifest> {
        let live = disk.create(&name("live"), CHUNK * 4).await?;
        live.write_chunk(ChunkIndex(0), fill(1)).await?;
        live.write_chunk(ChunkIndex(1), fill(2)).await?;
        let (c1, m1) = checkpoint(disk, store, &live, &name("c1"), None).await?;
        live.write_chunk(ChunkIndex(1), fill(9)).await?;
        live.zero_chunk(ChunkIndex(0)).await?;
        live.write_chunk(ChunkIndex(3), fill(5)).await?;
        let (_c2, m2) = checkpoint(disk, store, &live, &name("c2"), Some((&c1, &m1))).await?;
        disk.delete(&name("c1")).await?;
        Ok(m2)
    }

    /// After a cut anywhere in the flow, the live volume is intact: a
    /// fresh full checkpoint of it matches what the uncut flow ended at,
    /// up to the writes the cut prevented.
    #[tokio::test]
    async fn a_checkpoint_cut_at_any_op_never_damages_the_live_volume() {
        let dir = tempfile::tempdir().unwrap();
        let disk = CrashDisk::recording(&dir.path().join("volumes"), CHUNK).unwrap();
        let uncut = checkpoint_flow(disk.as_ref(), &store(dir.path()))
            .await
            .unwrap();
        let ops = disk.trace().len();
        assert_eq!(
            disk.trace(),
            vec![
                DiskOp::Create,
                DiskOp::Snapshot,
                DiskOp::Snapshot,
                DiskOp::DirtyChunks,
                DiskOp::Delete
            ]
        );

        for k in 0..ops {
            let dir = tempfile::tempdir().unwrap();
            let volumes = dir.path().join("volumes");
            let store = store(dir.path());
            let cut = CrashDisk::with_crash_at(&volumes, CHUNK, Some(k)).unwrap();
            let err = checkpoint_flow(cut.as_ref(), &store).await.unwrap_err();
            assert!(err.to_string().contains("injected crash"), "k={k}: {err}");

            if k == 0 {
                continue; // nothing was created
            }
            let recovered = FileDisk::new(&volumes, CHUNK).unwrap();
            let live = recovered.open(&name("live")).await.unwrap();
            let (_snap, redone) = checkpoint(&recovered, &store, &live, &name("redo"), None)
                .await
                .unwrap();
            // Cut before the second checkpoint: only the first writes landed.
            let expected: Vec<u64> = if k < 2 {
                vec![0, CHUNK]
            } else {
                uncut.chunks.iter().map(|c| c.offset).collect()
            };
            assert_eq!(
                redone.chunks.iter().map(|c| c.offset).collect::<Vec<_>>(),
                expected,
                "k={k}"
            );
            if k >= 2 {
                assert_eq!(redone.chunks, uncut.chunks, "k={k}");
            }
        }
    }

    /// A stage cut at any op leaves no usable volume under that name;
    /// staging again under a new name reproduces the manifest.
    #[tokio::test]
    async fn a_stage_cut_at_any_op_is_redone_under_a_new_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let source = FileDisk::new(dir.path().join("source"), CHUNK).unwrap();
        let live = source.create(&name("live"), CHUNK * 3).await.unwrap();
        live.write_chunk(ChunkIndex(2), fill(7)).await.unwrap();
        let (_snap, manifest) = checkpoint(&source, &store, &live, &name("c"), None)
            .await
            .unwrap();

        let volumes = dir.path().join("volumes");
        let recording = CrashDisk::recording(&volumes, CHUNK).unwrap();
        stage(recording.as_ref(), &store, &name("ok"), &manifest, None)
            .await
            .unwrap();
        assert_eq!(recording.trace(), vec![DiskOp::Create]);

        let cut = CrashDisk::with_crash_at(&volumes, CHUNK, Some(0)).unwrap();
        stage(cut.as_ref(), &store, &name("cut"), &manifest, None)
            .await
            .unwrap_err();
        assert!(!volumes.join("cut").exists());

        let redo = stage(
            &FileDisk::new(&volumes, CHUNK).unwrap(),
            &store,
            &name("redo"),
            &manifest,
            None,
        )
        .await
        .unwrap();
        assert_eq!(redo.read_chunk(ChunkIndex(2)).await.unwrap(), fill(7));
    }

    fn name(s: &str) -> VolumeName {
        VolumeName::new(s).expect("valid name")
    }

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
