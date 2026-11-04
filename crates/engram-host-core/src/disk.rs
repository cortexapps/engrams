//! Session disks the kernel owns (ADR 0125).
//!
//! A [`Volume`] is a block device or a file the hypervisor opens
//! directly; it is one concrete type because every platform reads,
//! writes, punches and syncs it the same way. [`SessionDisk`] is the
//! per-platform lifecycle around it. Everything is addressed in chunks
//! of `chunk_size` bytes; an all-zero chunk is a hole and is never
//! listed in a manifest.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

/// The name a volume is created and reopened by.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct VolumeName(String);

impl VolumeName {
    /// `[a-z0-9-]`, 1..=64 bytes: accepted verbatim by device-mapper
    /// and file systems.
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

/// Room left on the platform's store. A platform with no separate
/// metadata pool reports metadata equal to data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    pub data_free_bytes: u64,
    pub data_total_bytes: u64,
    pub metadata_free_bytes: u64,
    pub metadata_total_bytes: u64,
}

/// An open volume. Clones share the file handle.
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
    /// Open the device or file at `path` read-write.
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

    /// Number of chunks, including a short final one.
    pub fn chunk_count(&self) -> u64 {
        self.size_bytes.div_ceil(self.chunk_size)
    }

    /// Byte length of chunk `index`; only the final chunk can be short.
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

    /// Read chunk `index` in full.
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

    /// Write chunk `index` in full; `bytes` must be exactly its length.
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

    /// Make chunk `index` read as zeros and hold no space.
    pub async fn zero_chunk(&self, index: ChunkIndex) -> io::Result<()> {
        let offset = self.offset_of(index)?;
        let len = self.chunk_len(index)? as u64;
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || punch_hole(&file, offset, len))
            .await
            .map_err(|e| io::Error::other(format!("zero_chunk task: {e}")))?
    }

    /// Flush the host page cache for this volume to the store.
    pub async fn sync(&self) -> io::Result<()> {
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || file.sync_all())
            .await
            .map_err(|e| io::Error::other(format!("sync task: {e}")))?
    }
}

/// A chunk of zeros is a hole and is left out of manifests.
pub fn is_zero_chunk(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| b == 0)
}

/// Release `length` bytes at `offset` so they read as zeros: `BLKDISCARD`
/// on a block device (dm-thin does not accept `fallocate`'s write-zeroes
/// mapping; verify when the thin-pool implementation lands), `fallocate`
/// punch-hole on a file.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub fn punch_hole(file: &File, offset: u64, length: u64) -> io::Result<()> {
    if file.metadata()?.file_type().is_block_device() {
        // _IO(0x12, 119); argument is [start, length] in bytes.
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

/// Release `length` bytes at `offset` so they read as zeros. The range
/// must be block-aligned.
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

/// The platform's volume store.
#[async_trait]
pub trait SessionDisk: Send + Sync {
    /// A new volume of `size_bytes` that reads as zeros.
    async fn create(&self, name: &VolumeName, size_bytes: u64) -> io::Result<Volume>;

    /// An O(1) point-in-time copy of `origin`. A live origin must be
    /// paused and synced first.
    async fn snapshot(&self, origin: &Volume, name: &VolumeName) -> io::Result<Volume>;

    /// A volume that already exists. `NotFound` when it does not.
    async fn open(&self, name: &VolumeName) -> io::Result<Volume>;

    /// Remove a volume. Fails while a process holds it open.
    async fn delete(&self, name: &VolumeName) -> io::Result<()>;

    async fn capacity(&self) -> io::Result<Capacity>;

    /// Ascending indices of the chunks that differ between two snapshots
    /// of one origin. The default reads both in full and is the oracle a
    /// metadata-based override is tested against.
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

/// Volumes as sparse files in one directory: the macOS implementation
/// (`fs::copy` is a clone on APFS) and the Linux one without a thin pool.
#[derive(Clone, Debug)]
pub struct FileDisk {
    root: PathBuf,
    chunk_size: u64,
}

impl FileDisk {
    pub fn new(root: impl Into<PathBuf>, chunk_size: u64) -> io::Result<Self> {
        assert!(chunk_size > 0, "chunk_size must be non-zero");
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root, chunk_size })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_of(&self, name: &VolumeName) -> PathBuf {
        self.root.join(name.as_str())
    }
}

#[async_trait]
impl SessionDisk for FileDisk {
    async fn create(&self, name: &VolumeName, size_bytes: u64) -> io::Result<Volume> {
        let path = self.path_of(name);
        File::create_new(&path)?.set_len(size_bytes)?;
        Volume::open(name.clone(), &path, size_bytes, self.chunk_size)
    }

    async fn snapshot(&self, origin: &Volume, name: &VolumeName) -> io::Result<Volume> {
        let path = self.path_of(name);
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("volume {name} already exists"),
            ));
        }
        let (from, to) = (origin.path.clone(), path.clone());
        tokio::task::spawn_blocking(move || std::fs::copy(from, to))
            .await
            .map_err(|e| io::Error::other(format!("snapshot task: {e}")))??;
        Volume::open(name.clone(), &path, origin.size_bytes, origin.chunk_size)
    }

    async fn open(&self, name: &VolumeName) -> io::Result<Volume> {
        let path = self.path_of(name);
        let size_bytes = std::fs::metadata(&path)?.len();
        Volume::open(name.clone(), &path, size_bytes, self.chunk_size)
    }

    async fn delete(&self, name: &VolumeName) -> io::Result<()> {
        std::fs::remove_file(self.path_of(name))
    }

    async fn capacity(&self) -> io::Result<Capacity> {
        let (free, total) = filesystem_space(&self.root)?;
        Ok(Capacity {
            data_free_bytes: free,
            data_total_bytes: total,
            metadata_free_bytes: free,
            metadata_total_bytes: total,
        })
    }
}

/// Free and total bytes of the filesystem holding `path`.
#[allow(unsafe_code)]
fn filesystem_space(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `statvfs` reads a NUL-terminated path and fills the
    // out-parameter in full on success. Both live for the whole call.
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let result = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a zero return means the kernel initialised every field.
    let stat = unsafe { stat.assume_init() };
    // Block counts are 32-bit on macOS and 64-bit on Linux.
    fn widen(n: impl Into<u64>) -> u64 {
        n.into()
    }
    let frag = widen(stat.f_frsize);
    Ok((
        widen(stat.f_bavail).saturating_mul(frag),
        widen(stat.f_blocks).saturating_mul(frag),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: u64 = 4096;

    fn disk() -> (tempfile::TempDir, FileDisk) {
        let dir = tempfile::tempdir().expect("tempdir");
        let disk = FileDisk::new(dir.path().join("volumes"), CHUNK).expect("disk");
        (dir, disk)
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
        let (_dir, disk) = disk();
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
        let (_dir, disk) = disk();
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
        let (_dir, disk) = disk();
        let v = disk.create(&name("v"), CHUNK).await.unwrap();
        let err = v.read_chunk(ChunkIndex(1)).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let err = v.zero_chunk(ChunkIndex(u64::MAX)).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn zero_chunk_makes_a_written_chunk_read_as_zeros() {
        let (_dir, disk) = disk();
        let v = disk.create(&name("v"), CHUNK * 2).await.unwrap();
        v.write_chunk(ChunkIndex(0), vec![7; CHUNK as usize])
            .await
            .unwrap();
        v.zero_chunk(ChunkIndex(0)).await.unwrap();
        assert!(is_zero_chunk(&v.read_chunk(ChunkIndex(0)).await.unwrap()));
        v.sync().await.unwrap();
    }

    #[tokio::test]
    async fn open_finds_a_created_volume_and_delete_removes_it() {
        let (_dir, disk) = disk();
        disk.create(&name("v"), CHUNK * 2).await.unwrap();
        let v = disk.open(&name("v")).await.unwrap();
        assert_eq!(v.size_bytes, CHUNK * 2);
        disk.delete(&name("v")).await.unwrap();
        assert_eq!(
            disk.open(&name("v")).await.unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        // The name is free again.
        assert!(disk.create(&name("v"), CHUNK).await.is_ok());
    }

    #[tokio::test]
    async fn a_snapshot_is_independent_of_its_origin() {
        let (_dir, disk) = disk();
        let origin = disk.create(&name("o"), CHUNK * 2).await.unwrap();
        origin
            .write_chunk(ChunkIndex(0), vec![1; CHUNK as usize])
            .await
            .unwrap();
        let snap = disk.snapshot(&origin, &name("s")).await.unwrap();
        origin
            .write_chunk(ChunkIndex(0), vec![2; CHUNK as usize])
            .await
            .unwrap();
        assert_eq!(snap.read_chunk(ChunkIndex(0)).await.unwrap()[0], 1);
        assert_eq!(
            disk.snapshot(&origin, &name("s")).await.unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[tokio::test]
    async fn capacity_reports_the_underlying_filesystem() {
        let (_dir, disk) = disk();
        let c = disk.capacity().await.unwrap();
        assert!(c.data_total_bytes > 0);
        assert!(c.data_free_bytes <= c.data_total_bytes);
    }

    #[tokio::test]
    async fn dirty_chunks_refuses_volumes_of_different_sizes() {
        let (_dir, disk) = disk();
        let a = disk.create(&name("a"), CHUNK * 2).await.unwrap();
        let b = disk.create(&name("b"), CHUNK * 3).await.unwrap();
        let err = disk.dirty_chunks(&a, &b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn dirty_chunks_lists_exactly_the_chunks_that_changed() {
        let (_dir, disk) = disk();
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
