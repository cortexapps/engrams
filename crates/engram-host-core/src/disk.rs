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
    /// `[a-z0-9-]`, 1..=127 bytes: accepted verbatim by device-mapper
    /// (whose names are under 128 bytes) and file systems.
    pub fn new(name: impl Into<String>) -> Result<Self, String> {
        let name = name.into();
        let ok_len = (1..=127).contains(&name.len());
        let ok_chars = name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if ok_len && ok_chars {
            Ok(Self(name))
        } else {
            Err(format!(
                "invalid volume name {name:?}: want 1..=127 of [a-z0-9-]"
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

/// Room left in the platform's pool. All zeros means the platform has
/// no bounded pool (sparse files on a filesystem); placement treats
/// that as unmeasured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    pub data_free_bytes: u64,
    pub data_total_bytes: u64,
    pub metadata_free_bytes: u64,
    pub metadata_total_bytes: u64,
}

impl Capacity {
    /// Free data space is under `floor_bytes`. A disk with no pool
    /// reports no total and is never below a floor.
    pub fn data_below(&self, floor_bytes: u64) -> bool {
        self.data_total_bytes > 0 && self.data_free_bytes < floor_bytes
    }
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
    /// The same device opened for direct I/O, where the platform and the
    /// filesystem allow it. Volume data the host writes goes through it,
    /// so the host leaves no dirty pages behind: block-device writeback
    /// of a bio-based device such as dm-thin issues page-sized writes, and
    /// a close that must flush them can block for minutes.
    direct: Option<Arc<File>>,
}

/// Alignment for a direct write's offset, length and buffer.
const DIRECT_ALIGN: usize = 4096;

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
            direct: open_direct(path).map(Arc::new),
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
    pub async fn write_chunk(
        &self,
        index: ChunkIndex,
        bytes: impl AsRef<[u8]> + Send + 'static,
    ) -> io::Result<()> {
        let offset = self.offset_of(index)?;
        let len = self.chunk_len(index)?;
        if bytes.as_ref().len() != len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "chunk {} of volume {} is {len} bytes, got {}",
                    index.0,
                    self.name,
                    bytes.as_ref().len()
                ),
            ));
        }
        self.write_at(offset, bytes, "write_chunk").await
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

    /// Read `len` bytes at `offset`; the range must lie inside the volume.
    pub async fn read_range(&self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        self.check_range(offset, len)?;
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; len as usize];
            file.read_exact_at(&mut buf, offset)?;
            Ok(buf)
        })
        .await
        .map_err(|e| io::Error::other(format!("read_range task: {e}")))?
    }

    /// Write `bytes` at `offset`; the range must lie inside the volume.
    pub async fn write_range(&self, offset: u64, bytes: Vec<u8>) -> io::Result<()> {
        self.check_range(offset, bytes.len() as u64)?;
        self.write_at(offset, bytes, "write_range").await
    }

    /// Write through the direct handle when `offset` and the length are
    /// aligned for it, otherwise through the page cache.
    async fn write_at(
        &self,
        offset: u64,
        bytes: impl AsRef<[u8]> + Send + 'static,
        what: &'static str,
    ) -> io::Result<()> {
        let aligned = offset.is_multiple_of(DIRECT_ALIGN as u64)
            && bytes.as_ref().len().is_multiple_of(DIRECT_ALIGN);
        let direct = self.direct.as_ref().filter(|_| aligned).map(Arc::clone);
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || {
            let bytes = bytes.as_ref();
            match direct.zip(AlignedBuf::copy_of(bytes)) {
                Some((direct, buf)) => direct.write_all_at(buf.as_slice(), offset),
                None => file.write_all_at(bytes, offset),
            }
        })
        .await
        .map_err(|e| io::Error::other(format!("{what} task: {e}")))?
    }

    fn check_range(&self, offset: u64, len: u64) -> io::Result<()> {
        match offset.checked_add(len) {
            Some(end) if end <= self.size_bytes => Ok(()),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "range {offset}+{len} is past the end of volume {} ({} bytes)",
                    self.name, self.size_bytes
                ),
            )),
        }
    }

    /// Flush the host page cache for this volume to the store.
    pub async fn sync(&self) -> io::Result<()> {
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || file.sync_all())
            .await
            .map_err(|e| io::Error::other(format!("sync task: {e}")))?
    }
}

/// `path` opened read-write for direct I/O, or `None` where the platform
/// or the filesystem refuses it (tmpfs, macOS).
#[cfg(target_os = "linux")]
fn open_direct(path: &Path) -> Option<File> {
    use std::os::unix::fs::OpenOptionsExt;
    File::options()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn open_direct(_path: &Path) -> Option<File> {
    None
}

/// A copy of some bytes at an address aligned for direct I/O: a buffer
/// with `DIRECT_ALIGN` spare bytes, sliced at its first aligned address.
struct AlignedBuf {
    buf: Vec<u8>,
    start: usize,
    len: usize,
}

impl AlignedBuf {
    /// `None` when the allocation offers no aligned address.
    fn copy_of(bytes: &[u8]) -> Option<Self> {
        let mut buf = vec![0u8; bytes.len() + DIRECT_ALIGN];
        let start = buf.as_ptr().align_offset(DIRECT_ALIGN);
        buf.get_mut(start..start.checked_add(bytes.len())?)?
            .copy_from_slice(bytes);
        Some(Self {
            buf,
            start,
            len: bytes.len(),
        })
    }

    fn as_slice(&self) -> &[u8] {
        &self.buf[self.start..self.start + self.len]
    }
}

/// Two volumes are snapshots of one origin when their shapes match.
pub fn same_origin(before: &Volume, after: &Volume) -> io::Result<()> {
    if before.size_bytes != after.size_bytes || before.chunk_size != after.chunk_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} ({} bytes) and {} ({} bytes) must be snapshots of one origin",
                before.name, before.size_bytes, after.name, after.size_bytes
            ),
        ));
    }
    Ok(())
}

/// How many `block_bytes` blocks make a chunk of `volume`.
pub fn blocks_per_chunk(volume: &Volume, block_bytes: u64) -> io::Result<u64> {
    if block_bytes == 0 || !volume.chunk_size.is_multiple_of(block_bytes) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "block size {block_bytes} does not divide chunk size {}",
                volume.chunk_size
            ),
        ));
    }
    Ok(volume.chunk_size / block_bytes)
}

/// Number of `block_bytes` blocks in `volume`, including a short final one.
pub fn block_count(volume: &Volume, block_bytes: u64) -> u64 {
    volume.size_bytes.div_ceil(block_bytes)
}

/// A chunk of zeros is a hole and is left out of manifests.
pub fn is_zero_chunk(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| b == 0)
}

/// Release `length` bytes at `offset` so they read as zeros: `BLKDISCARD`
/// on a block device (a dm-thin volume unmaps the range and reads back
/// zeros), `fallocate` punch-hole on a file.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub fn punch_hole(file: &File, offset: u64, length: u64) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    if file.metadata()?.file_type().is_block_device() {
        // _IO(0x12, 119); argument is [start, length] in bytes.
        const BLKDISCARD: libc::Ioctl = 0x1277;
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

    /// Every volume that exists, in no particular order.
    async fn list(&self) -> io::Result<Vec<VolumeName>>;

    /// Give a volume a new name atomically. `AlreadyExists` when `to`
    /// is taken. Open handles keep working under the new name.
    async fn rename(&self, from: &VolumeName, to: &VolumeName) -> io::Result<()>;

    async fn capacity(&self) -> io::Result<Capacity>;

    /// Ascending indices of the `block_bytes`-sized blocks that differ
    /// between two snapshots of one origin; `block_bytes` divides the
    /// chunk size. The default reads both in full and is the oracle a
    /// metadata-based override is tested against.
    async fn dirty_blocks(
        &self,
        before: &Volume,
        after: &Volume,
        block_bytes: u64,
    ) -> io::Result<Vec<u64>> {
        same_origin(before, after)?;
        let blocks_per_chunk = blocks_per_chunk(before, block_bytes)?;
        let mut dirty = Vec::new();
        for index in (0..before.chunk_count()).map(ChunkIndex) {
            let (before_chunk, after_chunk) =
                tokio::try_join!(before.read_chunk(index), after.read_chunk(index))?;
            let first = index.0 * blocks_per_chunk;
            let blocks = before_chunk
                .chunks(block_bytes as usize)
                .zip(after_chunk.chunks(block_bytes as usize));
            for (i, (b, a)) in blocks.enumerate() {
                if b != a {
                    dirty.push(first + i as u64);
                }
            }
        }
        Ok(dirty)
    }

    /// Ascending indices of the chunks that differ between two snapshots
    /// of one origin.
    async fn dirty_chunks(&self, before: &Volume, after: &Volume) -> io::Result<Vec<ChunkIndex>> {
        let chunk_size = before.chunk_size;
        let mut chunks: Vec<ChunkIndex> = self
            .dirty_blocks(before, after, chunk_size)
            .await?
            .into_iter()
            .map(ChunkIndex)
            .collect();
        chunks.dedup();
        Ok(chunks)
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

    async fn list(&self) -> io::Result<Vec<VolumeName>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if let Ok(name) = VolumeName::new(entry.file_name().to_string_lossy()) {
                names.push(name);
            }
        }
        Ok(names)
    }

    async fn rename(&self, from: &VolumeName, to: &VolumeName) -> io::Result<()> {
        let target = self.path_of(to);
        if target.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("volume {to} already exists"),
            ));
        }
        std::fs::rename(self.path_of(from), target)
    }

    async fn capacity(&self) -> io::Result<Capacity> {
        Ok(Capacity {
            data_free_bytes: 0,
            data_total_bytes: 0,
            metadata_free_bytes: 0,
            metadata_total_bytes: 0,
        })
    }
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
        assert!(VolumeName::new("a".repeat(127)).is_ok());
        assert!(VolumeName::new("a".repeat(128)).is_err());
    }

    #[test]
    fn an_aligned_copy_starts_on_the_boundary_and_keeps_the_bytes() {
        for len in [0, 1, 4095, 4096, 65536 + 7] {
            let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let buf = AlignedBuf::copy_of(&bytes).expect("a heap buffer has an aligned address");
            assert_eq!(buf.as_slice().as_ptr() as usize % DIRECT_ALIGN, 0);
            assert_eq!(buf.as_slice(), &bytes[..]);
        }
    }

    #[tokio::test]
    async fn unaligned_and_aligned_range_writes_both_land() {
        let (_dir, disk) = disk();
        let v = disk.create(&name("v"), 4 * CHUNK).await.unwrap();
        v.write_range(4096, vec![0xAA; 8192]).await.unwrap();
        v.write_range(3, vec![0xBB; 5]).await.unwrap();
        assert_eq!(v.read_range(4096, 8192).await.unwrap(), vec![0xAA; 8192]);
        assert_eq!(v.read_range(3, 5).await.unwrap(), vec![0xBB; 5]);
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
    async fn list_names_every_volume_and_rename_moves_one_atomically() {
        let (_dir, disk) = disk();
        disk.create(&name("a"), CHUNK).await.unwrap();
        let b = disk.create(&name("b"), CHUNK).await.unwrap();
        b.write_chunk(ChunkIndex(0), vec![7; CHUNK as usize])
            .await
            .unwrap();
        let mut names = disk.list().await.unwrap();
        names.sort_by(|x, y| x.as_str().cmp(y.as_str()));
        assert_eq!(names, vec![name("a"), name("b")]);

        assert_eq!(
            disk.rename(&name("b"), &name("a"))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        disk.rename(&name("b"), &name("c")).await.unwrap();
        assert_eq!(
            disk.open(&name("b")).await.unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let c = disk.open(&name("c")).await.unwrap();
        assert_eq!(
            c.read_chunk(ChunkIndex(0)).await.unwrap(),
            vec![7; CHUNK as usize]
        );
        assert_eq!(
            b.read_chunk(ChunkIndex(0)).await.unwrap(),
            vec![7; CHUNK as usize]
        );
    }

    #[tokio::test]
    async fn capacity_reports_no_pool() {
        let (_dir, disk) = disk();
        let c = disk.capacity().await.unwrap();
        assert_eq!(c.data_total_bytes, 0);
        assert_eq!(c.metadata_total_bytes, 0);
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
