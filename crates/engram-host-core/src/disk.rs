//! The session-disk seam: volumes the kernel owns.
//!
//! A [`Volume`] is a block device or a file that a hypervisor opens
//! directly. Nothing in the host-agent sits in its data path. Once a
//! volume exists, every platform treats it the same way: positional
//! reads and writes of whole chunks, hole punching, and `fsync`. So
//! `Volume` is one concrete type.
//!
//! What varies by platform is how a volume comes to exist, how it is
//! copied in O(1), how two copies are compared, and how much room is
//! left. That is [`SessionDisk`]: a thin pool on Linux, APFS clones on
//! macOS, memory in the simulator.
//!
//! Chunks are the unit of everything here. A chunk is `chunk_size`
//! bytes at `index * chunk_size`, the same unit the chunk store uploads.
//! A chunk that reads as all zeros is a *hole*: it is never listed in a
//! manifest and it holds no space in the volume. See ADR 0125 for the
//! reasoning.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

/// The name a volume is created and reopened by. Recorded in the
/// sandbox's `sandbox.json`; the platform maps it to a device or a file.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct VolumeName(String);

impl VolumeName {
    /// Names are a short, device-safe alphabet: `[a-z0-9-]`, 1..=64
    /// bytes. Device-mapper and file systems both accept them verbatim.
    pub fn new(name: impl Into<String>) -> Result<Self, String> {
        let name = name.into();
        let ok_len = (1..=64).contains(&name.len());
        let ok_chars = name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if ok_len && ok_chars {
            Ok(Self(name))
        } else {
            Err(format!(
                "invalid volume name {name:?}: want 1..=64 of [a-z0-9-]"
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for VolumeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Position of a chunk in a volume: byte offset `index * chunk_size`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkIndex(pub u64);

/// Room left on the platform's store, reported in the heartbeat so
/// placement can refuse a host that cannot hold another session.
/// Metadata is a separate pool on Linux; a platform with no separate
/// metadata reports it equal to data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    pub data_free_bytes: u64,
    pub data_total_bytes: u64,
    pub metadata_free_bytes: u64,
    pub metadata_total_bytes: u64,
}

/// An open volume. Cheap to clone: the file handle is shared.
#[derive(Clone, Debug)]
pub struct Volume {
    pub name: VolumeName,
    /// What the hypervisor opens.
    pub path: PathBuf,
    pub size_bytes: u64,
    pub chunk_size: u64,
    file: Arc<File>,
}

impl Volume {
    /// Open the device or file at `path` read-write. Platform
    /// implementations call this after creating or finding a volume.
    pub fn open(
        name: VolumeName,
        path: &Path,
        size_bytes: u64,
        chunk_size: u64,
    ) -> io::Result<Self> {
        assert!(chunk_size > 0, "chunk_size must be non-zero");
        let file = File::options().read(true).write(true).open(path)?;
        Ok(Self {
            name,
            path: path.to_path_buf(),
            size_bytes,
            chunk_size,
            file: Arc::new(file),
        })
    }

    /// Number of chunks, counting a short final chunk.
    pub fn chunk_count(&self) -> u64 {
        self.size_bytes.div_ceil(self.chunk_size)
    }

    /// Byte length of chunk `index`: `chunk_size`, or shorter for the
    /// final chunk of a volume whose size is not a multiple.
    pub fn chunk_len(&self, index: ChunkIndex) -> io::Result<usize> {
        let offset = self.offset_of(index)?;
        Ok((self.size_bytes - offset).min(self.chunk_size) as usize)
    }

    fn offset_of(&self, index: ChunkIndex) -> io::Result<u64> {
        let offset = index.0.saturating_mul(self.chunk_size);
        if offset >= self.size_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "chunk {} is past the end of volume {} ({} bytes)",
                    index.0, self.name, self.size_bytes
                ),
            ));
        }
        Ok(offset)
    }

    /// Read chunk `index` in full. Runs on the blocking pool.
    pub async fn read_chunk(&self, index: ChunkIndex) -> io::Result<Vec<u8>> {
        let offset = self.offset_of(index)?;
        let len = self.chunk_len(index)?;
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; len];
            file.read_exact_at(&mut buf, offset)?;
            Ok(buf)
        })
        .await
        .map_err(|e| io::Error::other(format!("read_chunk task: {e}")))?
    }

    /// Write chunk `index` in full. `bytes` must be exactly the chunk's
    /// length; a partial chunk write is a caller bug, not a short write.
    pub async fn write_chunk(&self, index: ChunkIndex, bytes: Vec<u8>) -> io::Result<()> {
        let offset = self.offset_of(index)?;
        let len = self.chunk_len(index)?;
        if bytes.len() != len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "chunk {} of volume {} is {len} bytes, got {}",
                    index.0,
                    self.name,
                    bytes.len()
                ),
            ));
        }
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || file.write_all_at(&bytes, offset))
            .await
            .map_err(|e| io::Error::other(format!("write_chunk task: {e}")))?
    }

    /// Make chunk `index` read as zeros and hold no space. On a thin
    /// volume this unmaps the blocks; on a file it punches a hole.
    pub async fn zero_chunk(&self, index: ChunkIndex) -> io::Result<()> {
        let offset = self.offset_of(index)?;
        let len = self.chunk_len(index)? as u64;
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || punch_hole(&file, offset, len))
            .await
            .map_err(|e| io::Error::other(format!("zero_chunk task: {e}")))?
    }

    /// Push the host page cache for this volume down to the platform
    /// store. Called inside a checkpoint pause, before the snapshot.
    pub async fn sync(&self) -> io::Result<()> {
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || file.sync_all())
            .await
            .map_err(|e| io::Error::other(format!("sync task: {e}")))?
    }
}

/// True when every byte is zero: the chunk is a hole and is left out
/// of manifests.
pub fn is_zero_chunk(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| b == 0)
}

/// Release `length` bytes at `offset` so they read as zeros.
///
/// A block device (a thin volume) takes `BLKDISCARD`, which unmaps the
/// range so the pool reclaims it and reads return zeros. A regular
/// file takes `fallocate` with punch-hole. The kernel maps `fallocate`
/// on a block device to a write-zeroes request that dm-thin is not
/// expected to accept; the device branch exists so this never depends
/// on that mapping. Confirm on a thin volume when the Linux
/// implementation lands.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub fn punch_hole(file: &File, offset: u64, length: u64) -> io::Result<()> {
    if file.metadata()?.file_type().is_block_device() {
        // `_IO(0x12, 119)`: the discard ioctl, start and length in bytes.
        const BLKDISCARD: libc::c_ulong = 0x1277;
        let range: [u64; 2] = [offset, length];
        // SAFETY: the kernel copies the two-element range in and writes
        // nothing back. The fd is owned by `file` for the whole call.
        let result = unsafe { libc::ioctl(file.as_raw_fd(), BLKDISCARD, range.as_ptr()) };
        return if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        };
    }
    // SAFETY: fallocate takes its arguments by value and writes nothing
    // back. The fd is owned by `file` for the whole call; the range was
    // bounded by the caller and `KEEP_SIZE` keeps the length unchanged.
    let result = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            offset as libc::off_t,
            length as libc::off_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Release `length` bytes at `offset` so they read as zeros. APFS
/// requires the range to be block-aligned; whole chunks always are.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn punch_hole(file: &File, offset: u64, length: u64) -> io::Result<()> {
    let mut request = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: offset as libc::off_t,
        fp_length: length as libc::off_t,
    };
    // SAFETY: the kernel copies the fully initialised request in and
    // writes nothing back; the pointer outlives the call. The fd is
    // owned by `file` for the whole call.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &mut request) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The platform's volume store. One implementation per platform; the
/// lifecycle code above it is shared.
#[async_trait]
pub trait SessionDisk: Send + Sync {
    /// A new volume of `size_bytes` that reads as zeros.
    async fn create(&self, name: &VolumeName, size_bytes: u64) -> io::Result<Volume>;

    /// An O(1) point-in-time copy of `origin`. The origin may be in use
    /// by a running guest when the caller has paused the guest and
    /// called [`Volume::sync`] first.
    async fn snapshot(&self, origin: &Volume, name: &VolumeName) -> io::Result<Volume>;

    /// A volume that already exists. `NotFound` when it does not.
    async fn open(&self, name: &VolumeName) -> io::Result<Volume>;

    /// Remove a volume and return its exclusive space to the store.
    /// Fails while a process holds the volume open.
    async fn delete(&self, name: &VolumeName) -> io::Result<()>;

    async fn capacity(&self) -> io::Result<Capacity>;

    /// Indices of the chunks whose bytes differ between `before` and `after`,
    /// ascending. Both are snapshots of one origin. A platform with a
    /// metadata diff overrides this; the default reads both volumes in
    /// full and is the oracle the override is tested against.
    async fn dirty_chunks(&self, before: &Volume, after: &Volume) -> io::Result<Vec<ChunkIndex>> {
        let chunk_count = before.chunk_count();
        if chunk_count != after.chunk_count() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "dirty_chunks: {} has {chunk_count} chunks but {} has {}; \
                     both must be snapshots of one origin",
                    before.name,
                    after.name,
                    after.chunk_count()
                ),
            ));
        }

        let mut dirty = Vec::new();
        for index in (0..chunk_count).map(ChunkIndex) {
            let (before_chunk, after_chunk) =
                tokio::try_join!(before.read_chunk(index), after.read_chunk(index))?;
            if before_chunk != after_chunk {
                dirty.push(index);
            }
        }
        Ok(dirty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: u64 = 4096;

    /// A `SessionDisk` over sparse files in a temp dir: the smallest
    /// implementation that exercises the shared code. Snapshot is a
    /// byte copy, which is enough to test the default `dirty_chunks`.
    struct TempDisk {
        dir: tempfile::TempDir,
    }

    impl TempDisk {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().expect("tempdir"),
            }
        }

        fn path_of(&self, name: &VolumeName) -> PathBuf {
            self.dir.path().join(name.as_str())
        }
    }

    #[async_trait]
    impl SessionDisk for TempDisk {
        async fn create(&self, name: &VolumeName, size_bytes: u64) -> io::Result<Volume> {
            let path = self.path_of(name);
            File::create_new(&path)?.set_len(size_bytes)?;
            Volume::open(name.clone(), &path, size_bytes, CHUNK)
        }

        async fn snapshot(&self, origin: &Volume, name: &VolumeName) -> io::Result<Volume> {
            let path = self.path_of(name);
            std::fs::copy(&origin.path, &path)?;
            Volume::open(name.clone(), &path, origin.size_bytes, origin.chunk_size)
        }

        async fn open(&self, name: &VolumeName) -> io::Result<Volume> {
            let path = self.path_of(name);
            let size_bytes = std::fs::metadata(&path)?.len();
            Volume::open(name.clone(), &path, size_bytes, CHUNK)
        }

        async fn delete(&self, name: &VolumeName) -> io::Result<()> {
            std::fs::remove_file(self.path_of(name))
        }

        async fn capacity(&self) -> io::Result<Capacity> {
            Ok(Capacity {
                data_free_bytes: u64::MAX,
                data_total_bytes: u64::MAX,
                metadata_free_bytes: u64::MAX,
                metadata_total_bytes: u64::MAX,
            })
        }
    }

    fn name(s: &str) -> VolumeName {
        VolumeName::new(s).expect("valid name")
    }

    #[test]
    fn volume_name_accepts_device_safe_names_only() {
        assert!(VolumeName::new("img-abc-v3").is_ok());
        assert!(VolumeName::new("").is_err());
        assert!(VolumeName::new("Upper").is_err());
        assert!(VolumeName::new("has/slash").is_err());
        assert!(VolumeName::new("a".repeat(65)).is_err());
    }

    #[tokio::test]
    async fn a_new_volume_reads_as_zeros_and_round_trips_a_write() {
        let disk = TempDisk::new();
        let v = disk.create(&name("v"), CHUNK * 3).await.unwrap();
        assert_eq!(v.chunk_count(), 3);
        assert!(is_zero_chunk(&v.read_chunk(ChunkIndex(1)).await.unwrap()));

        let payload = vec![0xAB; CHUNK as usize];
        v.write_chunk(ChunkIndex(1), payload.clone()).await.unwrap();
        assert_eq!(v.read_chunk(ChunkIndex(1)).await.unwrap(), payload);
        assert!(is_zero_chunk(&v.read_chunk(ChunkIndex(2)).await.unwrap()));
    }

    #[tokio::test]
    async fn the_final_chunk_is_short_when_size_is_not_a_multiple() {
        let disk = TempDisk::new();
        let v = disk.create(&name("v"), CHUNK * 2 + 100).await.unwrap();
        assert_eq!(v.chunk_count(), 3);
        assert_eq!(v.chunk_len(ChunkIndex(2)).unwrap(), 100);
        assert_eq!(v.read_chunk(ChunkIndex(2)).await.unwrap().len(), 100);
        // A full-size buffer for the short chunk is a caller bug.
        let err = v
            .write_chunk(ChunkIndex(2), vec![1; CHUNK as usize])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn an_index_past_the_end_is_refused_before_any_io() {
        let disk = TempDisk::new();
        let v = disk.create(&name("v"), CHUNK).await.unwrap();
        let err = v.read_chunk(ChunkIndex(1)).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let err = v.zero_chunk(ChunkIndex(u64::MAX)).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn zero_chunk_makes_a_written_chunk_read_as_zeros() {
        let disk = TempDisk::new();
        let v = disk.create(&name("v"), CHUNK * 2).await.unwrap();
        v.write_chunk(ChunkIndex(0), vec![7; CHUNK as usize])
            .await
            .unwrap();
        v.zero_chunk(ChunkIndex(0)).await.unwrap();
        assert!(is_zero_chunk(&v.read_chunk(ChunkIndex(0)).await.unwrap()));
        v.sync().await.unwrap();
    }

    #[tokio::test]
    async fn dirty_chunks_refuses_volumes_of_different_sizes() {
        let disk = TempDisk::new();
        let a = disk.create(&name("a"), CHUNK * 2).await.unwrap();
        let b = disk.create(&name("b"), CHUNK * 3).await.unwrap();
        let err = disk.dirty_chunks(&a, &b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn dirty_chunks_lists_exactly_the_chunks_that_changed() {
        let disk = TempDisk::new();
        let fill = |byte: u8| vec![byte; CHUNK as usize];

        // Six chunks: 0 and 1 unchanged (one a hole, one written),
        // 2 rewritten with different bytes, 3 rewritten with the same
        // bytes, 4 written then zeroed, 5 a hole that gets written.
        let before = disk.create(&name("before"), CHUNK * 6).await.unwrap();
        for index in [1, 2, 3, 4] {
            before
                .write_chunk(ChunkIndex(index), fill(index as u8))
                .await
                .unwrap();
        }

        let after = disk.snapshot(&before, &name("after")).await.unwrap();
        after.write_chunk(ChunkIndex(2), fill(0xFF)).await.unwrap();
        after.write_chunk(ChunkIndex(3), fill(3)).await.unwrap();
        after.zero_chunk(ChunkIndex(4)).await.unwrap();
        after.write_chunk(ChunkIndex(5), fill(5)).await.unwrap();

        let dirty = disk.dirty_chunks(&before, &after).await.unwrap();
        assert_eq!(dirty, vec![ChunkIndex(2), ChunkIndex(4), ChunkIndex(5)]);

        // The comparison is symmetric and an unchanged pair is clean.
        assert_eq!(disk.dirty_chunks(&after, &before).await.unwrap(), dirty);
        assert!(disk
            .dirty_chunks(&before, &before)
            .await
            .unwrap()
            .is_empty());
    }
}
