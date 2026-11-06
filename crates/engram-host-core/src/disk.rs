//! Session disks the kernel owns (ADR 0125).
//!
//! A [`Volume`] is a block device or a file the hypervisor opens
//! directly; it is one concrete type because every platform reads,
//! writes, punches and syncs it the same way. [`SessionDisk`] is the
//! per-platform lifecycle around it. Everything is addressed in chunks
//! of `chunk_size` bytes; an all-zero chunk is a hole and is never
//! listed in a manifest.
//!
//! A cloned drive (ADR 0127) is a volume the guest uses before its
//! bytes are local: reads of regions not yet copied go to a read-only
//! source, writes land in a destination volume, and a bitmap volume
//! records the copied regions until the cutover.

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

/// The unit a cloned drive copies in (ADR 0127): the pool's block size.
pub const CLONE_REGION_BYTES: u64 = 64 * 1024;

/// How a cloned drive copies the regions the guest has not written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hydration {
    /// Copy in the background from the start. Off, only guest writes
    /// copy regions until [`SessionDisk::enable_hydration`].
    pub enabled: bool,
    /// Regions the background copy holds in flight at most.
    pub threshold_regions: u32,
    /// Adjacent regions the background copy joins into one copy.
    pub batch_regions: u32,
}

/// Where a cloned drive is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloneState {
    /// Reads of uncopied regions still go to the source.
    Copying {
        hydrated_regions: u64,
        total_regions: u64,
        hydration_enabled: bool,
    },
    /// The drive maps the destination alone; the source and the bitmap
    /// are no longer read.
    CutOver,
}

impl CloneState {
    /// Every region is in the destination, so [`SessionDisk::cut_over`]
    /// is allowed.
    pub fn is_hydrated(&self) -> bool {
        match *self {
            Self::Copying {
                hydrated_regions,
                total_regions,
                ..
            } => hydrated_regions == total_regions,
            Self::CutOver => true,
        }
    }
}

/// An open volume. Its copies share the file handle.
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

    /// Open the device or file at `path` read-only. Writes fail.
    pub fn open_read_only(
        name: VolumeName,
        path: &Path,
        size_bytes: u64,
        chunk_size: u64,
    ) -> io::Result<Self> {
        assert!(chunk_size > 0, "chunk_size must be non-zero");
        Ok(Self {
            name,
            path: path.to_path_buf(),
            size_bytes,
            chunk_size,
            file: Arc::new(File::open(path)?),
            direct: None,
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
    async fn write_at(&self, offset: u64, bytes: Vec<u8>, what: &'static str) -> io::Result<()> {
        let aligned =
            offset.is_multiple_of(DIRECT_ALIGN as u64) && bytes.len().is_multiple_of(DIRECT_ALIGN);
        let direct = self.direct.as_ref().filter(|_| aligned).map(Arc::clone);
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || match direct.zip(AlignedBuf::copy_of(&bytes)) {
            Some((direct, buf)) => direct.write_all_at(buf.as_slice(), offset),
            None => file.write_all_at(&bytes, offset),
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

    /// A new drive `drive` that reads as `source` and writes into
    /// `dest`, with the copy bitmap on `bitmap` (ADR 0127). `source` is
    /// a read-only block device (a file on [`FileDisk`]) at least as large
    /// as `dest`; `bitmap`
    /// is a fresh volume that reads as zeros. The drive is as large as
    /// `dest`. `AlreadyExists` when `drive` is taken.
    async fn clone_drive(
        &self,
        drive: &VolumeName,
        dest: &Volume,
        bitmap: &Volume,
        source: &Path,
        hydration: Hydration,
    ) -> io::Result<Volume>;

    /// A drive [`Self::clone_drive`] made, before or after its cutover.
    /// `NotFound` when it does not exist.
    async fn open_clone(&self, drive: &VolumeName) -> io::Result<Volume>;

    async fn clone_state(&self, drive: &VolumeName) -> io::Result<CloneState>;

    /// Start the background copy of a drive made with hydration off.
    /// `InvalidInput` after the cutover.
    async fn enable_hydration(&self, drive: &VolumeName) -> io::Result<()>;

    /// Make a fully hydrated drive map its destination alone. The drive's
    /// path and every open handle stay valid; afterwards the caller can
    /// delete the bitmap and stop the source. `ResourceBusy` while a
    /// region is not yet copied. A drive already cut over is left as it
    /// is.
    async fn cut_over(&self, drive: &VolumeName) -> io::Result<()>;

    /// Point-in-time copies of a drive's destination and bitmap that
    /// agree with each other: no region is copied between the two. The
    /// guest must be paused and the drive synced first, so the bitmap
    /// records every region the guest wrote. Returns `(dest, bitmap)`.
    /// `InvalidInput` after the cutover, when the destination alone is
    /// the drive.
    async fn snapshot_clone(
        &self,
        drive: &VolumeName,
        dest: &Volume,
        bitmap: &Volume,
        dest_snapshot: &VolumeName,
        bitmap_snapshot: &VolumeName,
    ) -> io::Result<(Volume, Volume)>;

    /// A read-only drive `view` over a [`Self::snapshot_clone`] pair and
    /// the drive's source. It reads exactly what the guest saw when the
    /// snapshots were taken, and copies nothing.
    async fn clone_view(
        &self,
        view: &VolumeName,
        dest_snapshot: &Volume,
        bitmap_snapshot: &Volume,
        source: &Path,
    ) -> io::Result<Volume>;

    /// Remove a drive or a view; its volumes stay. Fails while a process
    /// holds it open.
    async fn remove_clone(&self, drive: &VolumeName) -> io::Result<()>;

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
///
/// A cloned drive is a hard link to its destination under `.drives/`,
/// which [`SessionDisk::list`] does not read. `FileDisk` hydrates
/// eagerly: `clone_drive` copies the source into the destination before
/// it returns, so every drive is hydrated from the start and `cut_over`
/// only records the state. A view is a read-only link to the destination
/// snapshot, which already holds every byte the guest saw.
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

    /// `.` is not a volume-name character, so no volume shadows these.
    fn drives_dir(&self) -> PathBuf {
        self.root.join(".drives")
    }

    fn drive_path(&self, drive: &VolumeName) -> PathBuf {
        self.drives_dir().join(drive.as_str())
    }

    fn cut_over_marker(&self, drive: &VolumeName) -> PathBuf {
        self.drives_dir().join(format!("{drive}.cut-over"))
    }

    /// `drive` exists and is not cut over.
    fn copying(&self, drive: &VolumeName) -> io::Result<()> {
        std::fs::metadata(self.drive_path(drive))?;
        if self.cut_over_marker(drive).exists() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{drive} is cut over"),
            ));
        }
        Ok(())
    }

    /// Hard-link `volume`'s file as `drive`. `AlreadyExists` when taken.
    fn link_drive(&self, volume: &Volume, drive: &VolumeName) -> io::Result<PathBuf> {
        std::fs::create_dir_all(self.drives_dir())?;
        let path = self.drive_path(drive);
        std::fs::hard_link(&volume.path, &path).map_err(|e| {
            io::Error::new(e.kind(), format!("drive {drive} over {}: {e}", volume.name))
        })?;
        Ok(path)
    }
}

/// Make the first `len` bytes of `dest` equal those of `source`, writing
/// only the ranges that differ, then sync `dest`.
fn hydrate_file(source: &Path, dest: &File, len: u64) -> io::Result<()> {
    use std::io::{Seek, SeekFrom};
    const STEP: u64 = 1 << 20;
    let mut source_file = File::open(source)?;
    let source_len = source_file.seek(SeekFrom::End(0))?;
    if source_len < len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "source {} has {source_len} bytes; the drive needs {len}",
                source.display()
            ),
        ));
    }
    let mut want = vec![0u8; STEP as usize];
    let mut have = vec![0u8; STEP as usize];
    let mut offset = 0;
    while offset < len {
        let n = (len - offset).min(STEP) as usize;
        source_file.read_exact_at(&mut want[..n], offset)?;
        dest.read_exact_at(&mut have[..n], offset)?;
        if want[..n] != have[..n] {
            dest.write_all_at(&want[..n], offset)?;
        }
        offset += n as u64;
    }
    dest.sync_all()
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

    /// Copies `source` into `dest` before it returns; `hydration` has
    /// nothing to tune.
    async fn clone_drive(
        &self,
        drive: &VolumeName,
        dest: &Volume,
        _bitmap: &Volume,
        source: &Path,
        _hydration: Hydration,
    ) -> io::Result<Volume> {
        if self.drive_path(drive).exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("drive {drive} already exists"),
            ));
        }
        let (file, len, from) = (
            Arc::clone(&dest.file),
            dest.size_bytes,
            source.to_path_buf(),
        );
        tokio::task::spawn_blocking(move || hydrate_file(&from, &file, len))
            .await
            .map_err(|e| io::Error::other(format!("clone task: {e}")))??;
        let path = self.link_drive(dest, drive)?;
        Volume::open(drive.clone(), &path, dest.size_bytes, self.chunk_size)
    }

    async fn open_clone(&self, drive: &VolumeName) -> io::Result<Volume> {
        let path = self.drive_path(drive);
        let size_bytes = std::fs::metadata(&path)?.len();
        Volume::open(drive.clone(), &path, size_bytes, self.chunk_size)
    }

    async fn clone_state(&self, drive: &VolumeName) -> io::Result<CloneState> {
        let size_bytes = std::fs::metadata(self.drive_path(drive))?.len();
        if self.cut_over_marker(drive).exists() {
            return Ok(CloneState::CutOver);
        }
        let regions = size_bytes.div_ceil(CLONE_REGION_BYTES);
        Ok(CloneState::Copying {
            hydrated_regions: regions,
            total_regions: regions,
            hydration_enabled: true,
        })
    }

    async fn enable_hydration(&self, drive: &VolumeName) -> io::Result<()> {
        self.copying(drive)
    }

    async fn cut_over(&self, drive: &VolumeName) -> io::Result<()> {
        std::fs::metadata(self.drive_path(drive))?;
        File::create(self.cut_over_marker(drive)).map(drop)
    }

    async fn snapshot_clone(
        &self,
        drive: &VolumeName,
        dest: &Volume,
        bitmap: &Volume,
        dest_snapshot: &VolumeName,
        bitmap_snapshot: &VolumeName,
    ) -> io::Result<(Volume, Volume)> {
        self.copying(drive)?;
        let bitmap = self.snapshot(bitmap, bitmap_snapshot).await?;
        let dest = self.snapshot(dest, dest_snapshot).await?;
        Ok((dest, bitmap))
    }

    async fn clone_view(
        &self,
        view: &VolumeName,
        dest_snapshot: &Volume,
        _bitmap_snapshot: &Volume,
        _source: &Path,
    ) -> io::Result<Volume> {
        let path = self.link_drive(dest_snapshot, view)?;
        Volume::open_read_only(
            view.clone(),
            &path,
            dest_snapshot.size_bytes,
            self.chunk_size,
        )
    }

    async fn remove_clone(&self, drive: &VolumeName) -> io::Result<()> {
        std::fs::remove_file(self.drive_path(drive))?;
        match std::fs::remove_file(self.cut_over_marker(drive)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
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

    const REGION: u64 = CLONE_REGION_BYTES;
    const DRIVE: u64 = 4 * REGION;

    fn hydration() -> Hydration {
        Hydration {
            enabled: false,
            threshold_regions: 1,
            batch_regions: 1,
        }
    }

    /// A source file of `DRIVE` bytes where byte `i` is `i % 251 + 1`.
    fn source_file(dir: &Path) -> (PathBuf, Vec<u8>) {
        let bytes: Vec<u8> = (0..DRIVE).map(|i| (i % 251) as u8 + 1).collect();
        let path = dir.join("source.img");
        std::fs::write(&path, &bytes).unwrap();
        (path, bytes)
    }

    async fn write(volume: &Volume, expected: &mut [u8], offset: u64, bytes: Vec<u8>) {
        expected[offset as usize..offset as usize + bytes.len()].copy_from_slice(&bytes);
        volume.write_range(offset, bytes).await.unwrap();
    }

    async fn whole(volume: &Volume) -> Vec<u8> {
        volume.read_range(0, volume.size_bytes).await.unwrap()
    }

    #[tokio::test]
    async fn a_file_clone_reads_the_source_and_keeps_writes_through_the_cutover() {
        let (dir, disk) = disk();
        let (source, mut expected) = source_file(dir.path());
        let dest = disk.create(&name("dest"), DRIVE).await.unwrap();
        // Bytes the destination held before the clone do not show.
        dest.write_range(REGION, vec![0xEE; 4096]).await.unwrap();
        let bitmap = disk.create(&name("bitmap"), REGION).await.unwrap();

        let drive = disk
            .clone_drive(&name("drive"), &dest, &bitmap, &source, hydration())
            .await
            .unwrap();
        assert_eq!(whole(&drive).await, expected);
        assert_eq!(
            disk.clone_state(&name("drive")).await.unwrap(),
            CloneState::Copying {
                hydrated_regions: 4,
                total_regions: 4,
                hydration_enabled: true
            }
        );
        let mut names = disk.list().await.unwrap();
        names.sort_by(|x, y| x.as_str().cmp(y.as_str()));
        assert_eq!(names, vec![name("bitmap"), name("dest")]);

        write(&drive, &mut expected, REGION + 512, vec![0xAA; 4096]).await;
        write(
            &drive,
            &mut expected,
            2 * REGION,
            vec![0xBB; REGION as usize],
        )
        .await;
        disk.enable_hydration(&name("drive")).await.unwrap();
        disk.cut_over(&name("drive")).await.unwrap();
        disk.cut_over(&name("drive")).await.unwrap();
        assert_eq!(
            disk.clone_state(&name("drive")).await.unwrap(),
            CloneState::CutOver
        );
        assert_eq!(
            disk.enable_hydration(&name("drive"))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        disk.delete(&name("bitmap")).await.unwrap();

        write(&drive, &mut expected, 3 * REGION, vec![0xCC; 100]).await;
        assert_eq!(whole(&drive).await, expected);
        assert_eq!(whole(&dest).await, expected);
        let reopened = disk.open_clone(&name("drive")).await.unwrap();
        assert_eq!(reopened.path, drive.path);
        assert_eq!(whole(&reopened).await, expected);

        disk.remove_clone(&name("drive")).await.unwrap();
        assert_eq!(
            disk.clone_state(&name("drive")).await.unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            whole(&disk.open(&name("dest")).await.unwrap()).await,
            expected
        );
    }

    #[tokio::test]
    async fn a_file_view_reads_what_the_guest_saw_at_the_snapshot() {
        let (dir, disk) = disk();
        let (source, mut expected) = source_file(dir.path());
        let dest = disk.create(&name("dest"), DRIVE).await.unwrap();
        let bitmap = disk.create(&name("bitmap"), REGION).await.unwrap();
        let drive = disk
            .clone_drive(&name("drive"), &dest, &bitmap, &source, hydration())
            .await
            .unwrap();
        write(&drive, &mut expected, 4096, vec![1; 8192]).await;
        drive.sync().await.unwrap();

        let (dest_snap, bitmap_snap) = disk
            .snapshot_clone(
                &name("drive"),
                &dest,
                &bitmap,
                &name("dest-snap"),
                &name("bitmap-snap"),
            )
            .await
            .unwrap();
        drive.write_range(0, vec![2; 8192]).await.unwrap();

        let view = disk
            .clone_view(&name("view"), &dest_snap, &bitmap_snap, &source)
            .await
            .unwrap();
        assert_eq!(whole(&view).await, expected);
        assert!(view.write_range(0, vec![3; 512]).await.is_err());
        disk.remove_clone(&name("view")).await.unwrap();
        assert_eq!(whole(&dest_snap).await, expected);
    }

    #[tokio::test]
    async fn a_file_clone_refuses_a_short_source_and_a_taken_name() {
        let (dir, disk) = disk();
        let (source, _) = source_file(dir.path());
        let big = disk.create(&name("big"), 2 * DRIVE).await.unwrap();
        let dest = disk.create(&name("dest"), DRIVE).await.unwrap();
        let bitmap = disk.create(&name("bitmap"), REGION).await.unwrap();
        let err = disk
            .clone_drive(&name("drive"), &big, &bitmap, &source, hydration())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            disk.open_clone(&name("drive")).await.unwrap_err().kind(),
            io::ErrorKind::NotFound
        );

        disk.clone_drive(&name("drive"), &dest, &bitmap, &source, hydration())
            .await
            .unwrap();
        let err = disk
            .clone_drive(&name("drive"), &dest, &bitmap, &source, hydration())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    /// A [`FileDisk`] whose every [`SessionDisk`] call is traced and, from
    /// an armed index on, refused without effect: the volumes left behind
    /// are what a process death at that op leaves. Volume reads and
    /// writes are not cut; they go to the kernel in production too.
    struct CrashDisk {
        inner: FileDisk,
        trace: std::sync::Mutex<Vec<&'static str>>,
        crash_at: Option<usize>,
    }

    impl CrashDisk {
        fn new(root: &Path, crash_at: Option<usize>) -> Self {
            Self {
                inner: FileDisk::new(root, CHUNK).unwrap(),
                trace: std::sync::Mutex::new(Vec::new()),
                crash_at,
            }
        }

        fn trace(&self) -> Vec<&'static str> {
            self.trace.lock().unwrap().clone()
        }

        fn gate(&self, op: &'static str) -> io::Result<()> {
            let mut trace = self.trace.lock().unwrap();
            let index = trace.len();
            trace.push(op);
            if self.crash_at.is_some_and(|k| index >= k) {
                return Err(io::Error::other(format!(
                    "injected crash at op {index} ({op})"
                )));
            }
            Ok(())
        }
    }

    #[async_trait]
    impl SessionDisk for CrashDisk {
        async fn create(&self, name: &VolumeName, size_bytes: u64) -> io::Result<Volume> {
            self.gate("create")?;
            self.inner.create(name, size_bytes).await
        }
        async fn snapshot(&self, origin: &Volume, name: &VolumeName) -> io::Result<Volume> {
            self.gate("snapshot")?;
            self.inner.snapshot(origin, name).await
        }
        async fn open(&self, name: &VolumeName) -> io::Result<Volume> {
            self.gate("open")?;
            self.inner.open(name).await
        }
        async fn delete(&self, name: &VolumeName) -> io::Result<()> {
            self.gate("delete")?;
            self.inner.delete(name).await
        }
        async fn list(&self) -> io::Result<Vec<VolumeName>> {
            self.gate("list")?;
            self.inner.list().await
        }
        async fn rename(&self, from: &VolumeName, to: &VolumeName) -> io::Result<()> {
            self.gate("rename")?;
            self.inner.rename(from, to).await
        }
        async fn capacity(&self) -> io::Result<Capacity> {
            self.gate("capacity")?;
            self.inner.capacity().await
        }
        async fn clone_drive(
            &self,
            drive: &VolumeName,
            dest: &Volume,
            bitmap: &Volume,
            source: &Path,
            hydration: Hydration,
        ) -> io::Result<Volume> {
            self.gate("clone_drive")?;
            self.inner
                .clone_drive(drive, dest, bitmap, source, hydration)
                .await
        }
        async fn open_clone(&self, drive: &VolumeName) -> io::Result<Volume> {
            self.gate("open_clone")?;
            self.inner.open_clone(drive).await
        }
        async fn clone_state(&self, drive: &VolumeName) -> io::Result<CloneState> {
            self.gate("clone_state")?;
            self.inner.clone_state(drive).await
        }
        async fn enable_hydration(&self, drive: &VolumeName) -> io::Result<()> {
            self.gate("enable_hydration")?;
            self.inner.enable_hydration(drive).await
        }
        async fn cut_over(&self, drive: &VolumeName) -> io::Result<()> {
            self.gate("cut_over")?;
            self.inner.cut_over(drive).await
        }
        async fn snapshot_clone(
            &self,
            drive: &VolumeName,
            dest: &Volume,
            bitmap: &Volume,
            dest_snapshot: &VolumeName,
            bitmap_snapshot: &VolumeName,
        ) -> io::Result<(Volume, Volume)> {
            self.gate("snapshot_clone")?;
            self.inner
                .snapshot_clone(drive, dest, bitmap, dest_snapshot, bitmap_snapshot)
                .await
        }
        async fn clone_view(
            &self,
            view: &VolumeName,
            dest_snapshot: &Volume,
            bitmap_snapshot: &Volume,
            source: &Path,
        ) -> io::Result<Volume> {
            self.gate("clone_view")?;
            self.inner
                .clone_view(view, dest_snapshot, bitmap_snapshot, source)
                .await
        }
        async fn remove_clone(&self, drive: &VolumeName) -> io::Result<()> {
            self.gate("remove_clone")?;
            self.inner.remove_clone(drive).await
        }
    }

    async fn create_or_open(disk: &dyn SessionDisk, n: &str, size: u64) -> io::Result<Volume> {
        match disk.create(&name(n), size).await {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => disk.open(&name(n)).await,
            created => created,
        }
    }

    /// A lazy restore and its cutover, written to converge from any state
    /// an earlier pass left: the drive's state decides what is left to
    /// do. The guest's writes are the same on every pass.
    async fn lazy_restore(disk: &dyn SessionDisk, source: &Path) -> io::Result<Volume> {
        let drive = match disk.clone_state(&name("drive")).await {
            Ok(_) => disk.open_clone(&name("drive")).await?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let dest = create_or_open(disk, "dest", DRIVE).await?;
                let bitmap = create_or_open(disk, "bitmap", REGION).await?;
                disk.clone_drive(&name("drive"), &dest, &bitmap, source, hydration())
                    .await?
            }
            Err(e) => return Err(e),
        };
        drive.write_range(REGION + 512, vec![0xAA; 4096]).await?;
        drive
            .write_range(2 * REGION, vec![0xBB; REGION as usize])
            .await?;
        if !disk.clone_state(&name("drive")).await?.is_hydrated() {
            return Err(io::Error::other("the drive is not hydrated"));
        }
        disk.cut_over(&name("drive")).await?;
        match disk.delete(&name("bitmap")).await {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        Ok(drive)
    }

    #[tokio::test]
    async fn a_lazy_restore_cut_at_any_op_converges_on_the_next_pass() {
        let dir = tempfile::tempdir().unwrap();
        let (source, mut expected) = source_file(dir.path());
        expected[(REGION + 512) as usize..(REGION + 512 + 4096) as usize].fill(0xAA);
        expected[(2 * REGION) as usize..(3 * REGION) as usize].fill(0xBB);

        let recording = CrashDisk::new(&dir.path().join("recorded"), None);
        lazy_restore(&recording, &source).await.unwrap();
        assert_eq!(
            recording.trace(),
            vec![
                "clone_state",
                "create",
                "create",
                "clone_drive",
                "clone_state",
                "cut_over",
                "delete"
            ]
        );

        for k in 0..recording.trace().len() {
            let root = dir.path().join(format!("cut-{k}"));
            let cut = CrashDisk::new(&root, Some(k));
            let err = lazy_restore(&cut, &source).await.unwrap_err();
            assert!(err.to_string().contains("injected crash"), "k={k}: {err}");

            let disk = FileDisk::new(&root, CHUNK).unwrap();
            let drive = lazy_restore(&disk, &source).await.unwrap();
            assert_eq!(whole(&drive).await, expected, "k={k}");
            assert_eq!(
                whole(&disk.open(&name("dest")).await.unwrap()).await,
                expected,
                "k={k}"
            );
            assert_eq!(
                disk.clone_state(&name("drive")).await.unwrap(),
                CloneState::CutOver,
                "k={k}"
            );
            assert_eq!(disk.list().await.unwrap(), vec![name("dest")], "k={k}");
        }
    }
}
