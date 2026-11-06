//! The disk over the ioctl layer; Linux only.

use std::collections::HashMap;
use std::fs::File;
use std::future::Future;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::dm;
use crate::parse::{self, CloneTable, DevNo, ThinParams, METADATA_BLOCK_BYTES};
use async_trait::async_trait;
use engram_host_core::disk::{
    check_fresh_bitmap, discard, skip_ranges, Capacity, ChunkIndex, CloneState, Hydration,
    SessionDisk, Volume, VolumeName, CLONE_HEADER_BYTES, CLONE_REGION_BYTES,
};

const SECTOR_BYTES: u64 = 512;
const REGION_SECTORS: u64 = CLONE_REGION_BYTES / SECTOR_BYTES;
/// A view is loaded with hydration off and never turned on, so its copy
/// tuning has no effect.
const VIEW: Hydration = Hydration {
    enabled: false,
    threshold_regions: 1,
    batch_regions: 1,
};
/// dm-thin device ids are 24 bits wide.
const ID_MASK: u32 = 0x00FF_FFFF;
/// A `delete` waits this long in all for the last holder of a device
/// to close it.
const REMOVE_ATTEMPTS: u32 = 10;
const REMOVE_RETRY: Duration = Duration::from_millis(100);
/// A resume that fails is retried this many times in all before the
/// device is reported left suspended.
const RESUME_ATTEMPTS: u32 = 5;
const RESUME_RETRY: Duration = Duration::from_millis(50);

/// Copies share the pool's id counter, metadata-snapshot lock and drive
/// locks, so an operation can run in a task of its own.
#[derive(Clone)]
pub struct ThinPoolDisk {
    dm: dm::Dm,
    pool: String,
    pool_dev: DevNo,
    metadata_path: PathBuf,
    nodes_dir: PathBuf,
    chunk_size: u64,
    block_bytes: u64,
    /// Seeded from the pool at `open`. The kernel refuses a duplicate id
    /// and `create` and `snapshot` move on, so a wrap of the 24-bit space
    /// is safe.
    next_id: Arc<AtomicU32>,
    /// The pool holds one reserved metadata snapshot at a time.
    metadata_snap: Arc<tokio::sync::Mutex<()>>,
    /// One lock per drive or view name, held across each operation on it.
    drive_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl ThinPoolDisk {
    /// Attach to an active pool whose block size divides `chunk_size`,
    /// resume its devices that a process death left suspended, delete
    /// thins that have no device (a crash between `delete` and the pool
    /// message), and seed the id counter. Device nodes are made under
    /// `nodes_dir`. No other process may operate on the pool meanwhile.
    pub async fn open(pool: &str, chunk_size: u64, nodes_dir: &Path) -> io::Result<Self> {
        let dm = dm::Dm::open()?;
        let params = parse::pool_params(&single_target(&dm.table(pool).await?, "thin-pool")?)?;
        let block_bytes = params.block_sectors * SECTOR_BYTES;
        if chunk_size == 0 || !chunk_size.is_multiple_of(block_bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "pool {pool} has {block_bytes}-byte blocks, which do not divide chunk size {chunk_size}"
                ),
            ));
        }
        tokio::fs::create_dir_all(nodes_dir).await?;
        let disk = Self {
            pool_dev: dm.devno(pool).await?,
            metadata_path: dm::device_path(params.metadata_dev).await?,
            dm,
            pool: pool.to_string(),
            nodes_dir: nodes_dir.to_path_buf(),
            chunk_size,
            block_bytes,
            next_id: Arc::new(AtomicU32::new(1)),
            metadata_snap: Arc::new(tokio::sync::Mutex::new(())),
            drive_locks: Arc::new(Mutex::new(HashMap::new())),
        };
        disk.resume_suspended().await?;
        let in_pool = disk.thin_ids().await?;
        let active = disk.active_ids().await?;
        for id in in_pool.iter().filter(|id| !active.contains(id)) {
            tracing::warn!(pool, id, "deleting a thin that has no device");
            disk.dm.message(pool, &format!("delete {id}")).await?;
        }
        let next = in_pool.iter().max().map_or(1, |max| max + 1);
        disk.next_id.store(next & ID_MASK, Ordering::Relaxed);
        Ok(disk)
    }

    pub fn pool(&self) -> &str {
        &self.pool
    }

    /// Resume every device of this pool that is suspended: thins first,
    /// then the drives and views stacked on them.
    async fn resume_suspended(&self) -> io::Result<()> {
        let prefix = format!("{}.", self.pool);
        let mut suspended = Vec::new();
        for (name, _) in self.dm.list().await? {
            if !name.starts_with(&prefix) {
                continue;
            }
            match self.dm.info(&name).await {
                Ok(info) if info.suspended => {}
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => continue,
            }
            let thin = match self.dm.table(&name).await {
                Ok(targets) => targets.iter().any(|t| t.kind == "thin"),
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            suspended.push((!thin, name));
        }
        suspended.sort();
        for (_, name) in suspended {
            tracing::warn!(device = %name, "resuming a device a process death left suspended");
            self.dm.resume(&name).await?;
        }
        Ok(())
    }

    /// The lock of drive or view `name`.
    async fn lock_drive(&self, name: &VolumeName) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.drive_locks.lock().unwrap_or_else(|e| e.into_inner());
            locks.retain(|_, lock| Arc::strong_count(lock) > 1);
            Arc::clone(locks.entry(name.as_str().to_string()).or_default())
        };
        lock.lock_owned().await
    }

    /// Every thin id the pool holds, with or without a device.
    pub async fn thin_ids(&self) -> io::Result<Vec<u32>> {
        self.with_metadata_snap(dm::thin_dump_ids(&self.metadata_path))
            .await
    }

    /// The device-mapper name of a volume. Device names are global to the
    /// kernel, so each pool's volumes carry its name: two host-agents on
    /// one machine, each with its own pool, never share a device or a node.
    pub fn device_name(&self, name: &VolumeName) -> String {
        format!("{}.{}", self.pool, name.as_str())
    }

    fn node_of(&self, name: &VolumeName) -> PathBuf {
        self.nodes_dir.join(self.device_name(name))
    }

    async fn active_ids(&self) -> io::Result<Vec<u32>> {
        Ok(self
            .thins()
            .await?
            .into_iter()
            .map(|t| t.params.id)
            .collect())
    }

    /// Every thin device on this pool, from one device list and one
    /// table read per device. A device that goes away between the two
    /// is not on the pool.
    async fn thins(&self) -> io::Result<Vec<Thin>> {
        let mut thins = Vec::new();
        for (name, _) in self.dm.list().await? {
            let targets = match self.dm.table(&name).await {
                Ok(targets) => targets,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let [target] = targets.as_slice() else {
                continue;
            };
            if target.kind != "thin" {
                continue;
            }
            let params = parse::thin_params(&target.params)?;
            if params.pool == self.pool_dev {
                thins.push(Thin { name, params });
            }
        }
        Ok(thins)
    }

    /// The thin behind `name` and its size, refused when it belongs to
    /// another pool.
    async fn thin_of(&self, name: &VolumeName) -> io::Result<(ThinParams, u64)> {
        let targets = self.dm.table(&self.device_name(name)).await?;
        let params = parse::thin_params(&single_target(&targets, "thin")?)?;
        if params.pool != self.pool_dev {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{name} is a thin of pool {}, not {}",
                    params.pool, self.pool
                ),
            ));
        }
        Ok((params, targets[0].length_sectors))
    }

    /// A device over thin `id`. On failure the thin is deleted from the
    /// pool again, so a failed create or snapshot leaves nothing.
    async fn activate(&self, name: &VolumeName, id: u32, sectors: u64) -> io::Result<Volume> {
        let params = format!("{} {id}", self.pool_dev);
        let activated = async {
            self.dm
                .create(&self.device_name(name), sectors, "thin", &params)
                .await?;
            self.open_created(name, |node| {
                Volume::open(name.clone(), node, sectors * SECTOR_BYTES, self.chunk_size)
            })
            .await
        }
        .await;
        if activated.is_err() {
            if let Err(error) = self.dm.message(&self.pool, &format!("delete {id}")).await {
                tracing::warn!(pool = %self.pool, id, %error, "thin left behind by a failed activate");
            }
        }
        activated
    }

    /// The device node of `name`, made when it is missing or stale.
    async fn node(&self, name: &VolumeName) -> io::Result<PathBuf> {
        let node = self.node_of(name);
        dm::make_node(&node, self.dm.devno(&self.device_name(name)).await?)?;
        Ok(node)
    }

    async fn volume(&self, name: &VolumeName, sectors: u64) -> io::Result<Volume> {
        let node = self.node(name).await?;
        Volume::open(name.clone(), &node, sectors * SECTOR_BYTES, self.chunk_size)
    }

    /// Open a device just created through its node. On failure the node
    /// and the device are removed again.
    async fn open_created(
        &self,
        name: &VolumeName,
        open: impl FnOnce(&Path) -> io::Result<Volume>,
    ) -> io::Result<Volume> {
        let opened = match self.node(name).await {
            Ok(node) => open(&node),
            Err(e) => Err(e),
        };
        if opened.is_err() {
            let _ = tokio::fs::remove_file(self.node_of(name)).await;
            let _ = self.dm.remove(&self.device_name(name)).await;
        }
        opened
    }

    /// Remove `name`'s device, waiting a bounded time for its last holder
    /// to close it, then its node.
    async fn remove_device(&self, name: &VolumeName) -> io::Result<()> {
        let mut attempt = 1;
        loop {
            match self.dm.remove(&self.device_name(name)).await {
                Err(e) if e.kind() == io::ErrorKind::ResourceBusy && attempt < REMOVE_ATTEMPTS => {
                    attempt += 1;
                    tokio::time::sleep(REMOVE_RETRY).await;
                }
                removed => break removed?,
            }
        }
        match tokio::fs::remove_file(self.node_of(name)).await {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// What `name` is to the drive operations; `None` when it exists and
    /// is not a drive or a view.
    async fn drive_kind(&self, name: &VolumeName) -> io::Result<Option<Drive>> {
        let device = self.device_name(name);
        let targets = self.dm.table(&device).await?;
        Ok(match targets.as_slice() {
            [] => Some(Drive::Incomplete),
            [target] if target.kind == "linear" => Some(Drive::CutOver {
                sectors: target.length_sectors,
            }),
            [target] if target.kind == "clone" => {
                let table = parse::clone_table(&target.params)?;
                let sectors = target.length_sectors;
                Some(if self.dm.info(&device).await?.read_only {
                    Drive::View { sectors }
                } else {
                    Drive::Copying { table, sectors }
                })
            }
            _ => None,
        })
    }

    async fn drive(&self, name: &VolumeName) -> io::Result<Drive> {
        self.drive_kind(name).await?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} is not a cloned drive or a view"),
            )
        })
    }

    /// The clone table of a drive that copies; `InvalidInput` for any
    /// other drive or view.
    async fn copying_drive(&self, drive: &VolumeName) -> io::Result<(CloneTable, u64)> {
        match self.drive(drive).await? {
            Drive::Copying { table, sectors } => Ok((table, sectors)),
            other => Err(other.refused(drive)),
        }
    }

    /// A new empty thin with no device, by id.
    async fn create_thin(&self) -> io::Result<u32> {
        loop {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed) & ID_MASK;
            match self
                .dm
                .message(&self.pool, &format!("create_thin {id}"))
                .await
            {
                Ok(()) => return Ok(id),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// The `block_bytes` blocks of `volume` in which thins `left` and
    /// `right` map different pool blocks; `block_bytes` is a multiple of
    /// the pool's block size.
    async fn delta(
        &self,
        left: u32,
        right: u32,
        volume: &Volume,
        block_bytes: u64,
    ) -> io::Result<Vec<u64>> {
        if !block_bytes.is_multiple_of(self.block_bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "block size {block_bytes} is not a multiple of the pool's {} bytes",
                    self.block_bytes
                ),
            ));
        }
        let pool_blocks = self
            .with_metadata_snap(dm::thin_delta(&self.metadata_path, left, right))
            .await?;
        let per_block = block_bytes / self.block_bytes;
        let count = engram_host_core::disk::block_count(volume, block_bytes);
        let mut blocks: Vec<u64> = pool_blocks
            .into_iter()
            .map(|block| block / per_block)
            .filter(|block| *block < count)
            .collect();
        blocks.dedup();
        Ok(blocks)
    }

    /// A clone table over two thins of this pool and `source`.
    async fn clone_table(
        &self,
        dest: &Volume,
        bitmap: &Volume,
        source: &Path,
        region_sectors: u64,
    ) -> io::Result<(CloneTable, u64)> {
        let (_, sectors) = self.thin_of(&dest.name).await?;
        self.thin_of(&bitmap.name).await?;
        let table = CloneTable {
            metadata_dev: self.dm.devno(&self.device_name(&bitmap.name)).await?,
            dest_dev: self.dm.devno(&self.device_name(&dest.name)).await?,
            source_dev: dm::block_devno(source)?,
            region_sectors,
        };
        Ok((table, sectors))
    }

    /// The region counts of a `clone` target.
    async fn clone_status(&self, drive: &VolumeName) -> io::Result<CloneState> {
        let status = self.dm.status(&self.device_name(drive)).await?;
        parse::clone_status(&single_target(&status, "clone")?)
    }

    /// `ResourceBusy` while a device is stacked on `volume`.
    async fn check_unused(&self, volume: &VolumeName) -> io::Result<()> {
        let devno = self.dm.devno(&self.device_name(volume)).await?;
        if dm::holders(devno)? > 0 {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!("{volume} is used by a drive or a view"),
            ));
        }
        Ok(())
    }

    async fn cut_over_locked(&self, drive: &VolumeName) -> io::Result<()> {
        let (table, sectors) = match self.drive(drive).await? {
            Drive::CutOver { .. } => return Ok(()),
            Drive::Copying { table, sectors } => (table, sectors),
            other => return Err(other.refused(drive)),
        };
        if !self.clone_status(drive).await?.is_hydrated() {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!("{drive} is still hydrating"),
            ));
        }
        let device = self.device_name(drive);
        let suspended = Suspended::suspend(&self.dm, &device).await?;
        let loaded = self
            .dm
            .load(&device, sectors, "linear", &format!("{} 0", table.dest_dev))
            .await;
        let resumed = suspended.resume().await;
        loaded.and(resumed)
    }

    async fn snapshot_clone_locked(
        &self,
        drive: &VolumeName,
        dest: &Volume,
        bitmap: &Volume,
        dest_snapshot: &VolumeName,
        bitmap_snapshot: &VolumeName,
    ) -> io::Result<(Volume, Volume)> {
        let (table, _) = self.copying_drive(drive).await?;
        let dest_dev = self.dm.devno(&self.device_name(&dest.name)).await?;
        let bitmap_dev = self.dm.devno(&self.device_name(&bitmap.name)).await?;
        if table.dest_dev != dest_dev || table.metadata_dev != bitmap_dev {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{drive} copies into {} with bitmap {}, not {} and {}",
                    table.dest_dev, table.metadata_dev, dest.name, bitmap.name
                ),
            ));
        }
        let suspended = Suspended::suspend(&self.dm, &self.device_name(drive)).await?;
        let mut made = Vec::with_capacity(2);
        let taken = async {
            for (origin, name) in [(bitmap, bitmap_snapshot), (dest, dest_snapshot)] {
                made.push(self.snapshot(origin, name).await?);
            }
            Ok(())
        }
        .await;
        let resumed = suspended.resume().await;
        if let Err(e) = taken.and(resumed) {
            // The destination snapshot goes first: one left without its
            // bitmap snapshot is refused, never read as a whole disk.
            for snap in made.into_iter().rev() {
                let name = snap.name.clone();
                drop(snap);
                if let Err(error) = self.delete(&name).await {
                    tracing::warn!(volume = %name, %error, "snapshot left behind by a failed snapshot_clone");
                }
            }
            return Err(e);
        }
        let dest_snap = made.pop().expect("two snapshots taken");
        let bitmap_snap = made.pop().expect("two snapshots taken");
        Ok((dest_snap, bitmap_snap))
    }

    /// Run `read` over a reserved metadata snapshot. The reserve is
    /// released when the guard drops, so a cancelled read releases it
    /// too. A reserve another holder left behind (a process that died,
    /// a read cancelled before this guard existed) is released first.
    async fn with_metadata_snap<T>(
        &self,
        read: impl Future<Output = io::Result<T>>,
    ) -> io::Result<T> {
        let _held = self.metadata_snap.lock().await;
        if let Err(error) = self.dm.message(&self.pool, "reserve_metadata_snap").await {
            tracing::warn!(pool = %self.pool, %error, "metadata snapshot not reserved; releasing a stale one");
            self.dm.message(&self.pool, "release_metadata_snap").await?;
            self.dm.message(&self.pool, "reserve_metadata_snap").await?;
        }
        let _reserved = MetadataSnap {
            dm: &self.dm,
            pool: &self.pool,
        };
        read.await
    }

    fn sectors_of(size_bytes: u64) -> io::Result<u64> {
        if size_bytes == 0 || !size_bytes.is_multiple_of(SECTOR_BYTES) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("volume size {size_bytes} is not a positive multiple of {SECTOR_BYTES}"),
            ));
        }
        Ok(size_bytes / SECTOR_BYTES)
    }
}

struct Thin {
    name: String,
    params: ThinParams,
}

/// What a name is to the drive operations.
enum Drive {
    /// A read-write `clone` target: copying into its destination.
    Copying { table: CloneTable, sectors: u64 },
    /// A `linear` target onto the destination.
    CutOver { sectors: u64 },
    /// A read-only `clone` target: a checkpoint's view.
    View { sectors: u64 },
    /// No live table: a `clone_drive` that did not finish.
    Incomplete,
}

impl Drive {
    fn refused(&self, name: &VolumeName) -> io::Error {
        let what = match self {
            Self::Copying { .. } => "is copying",
            Self::CutOver { .. } => "is cut over",
            Self::View { .. } => "is a view",
            Self::Incomplete => "has no table",
        };
        io::Error::new(io::ErrorKind::InvalidInput, format!("{name} {what}"))
    }
}

/// A suspended device. Until a resume succeeds it stays armed: a drop
/// resumes it, so a task aborted between the two still resumes it.
struct Suspended {
    device: Option<(dm::Dm, String)>,
}

impl Suspended {
    async fn suspend(dm: &dm::Dm, name: &str) -> io::Result<Self> {
        dm.suspend(name).await?;
        Ok(Self {
            device: Some((dm.clone(), name.to_string())),
        })
    }

    /// Resume, retrying a bounded number of times. On failure the drop
    /// tries once more and logs the device as left suspended.
    async fn resume(mut self) -> io::Result<()> {
        let (dm, name) = self.device.as_ref().expect("armed until resumed");
        let mut attempt = 1;
        loop {
            match dm.resume(name).await {
                Ok(()) => break,
                Err(error) if attempt < RESUME_ATTEMPTS => {
                    tracing::warn!(device = %name, attempt, %error, "resume failed; retrying");
                    attempt += 1;
                    tokio::time::sleep(RESUME_RETRY).await;
                }
                Err(error) => return Err(error),
            }
        }
        self.device = None;
        Ok(())
    }
}

impl Drop for Suspended {
    fn drop(&mut self) {
        if let Some((dm, name)) = self.device.take() {
            if let Err(error) = dm.resume_blocking(&name) {
                tracing::error!(device = %name, %error, "device left suspended");
            }
        }
    }
}

/// Run `op` in a task of its own, so a caller that is dropped cannot stop
/// it between a suspend and the resume.
async fn detached<T: Send + 'static>(
    op: impl Future<Output = io::Result<T>> + Send + 'static,
) -> io::Result<T> {
    tokio::spawn(op).await.map_err(io::Error::other)?
}

/// A reserved metadata snapshot; released on drop.
struct MetadataSnap<'a> {
    dm: &'a dm::Dm,
    pool: &'a str,
}

impl Drop for MetadataSnap<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.dm.message_blocking(self.pool, "release_metadata_snap") {
            tracing::warn!(pool = self.pool, %error, "metadata snapshot not released");
        }
    }
}

fn single_target(targets: &[dm::Target], kind: &str) -> io::Result<String> {
    match targets {
        [target] if target.kind == kind => Ok(target.params.clone()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected one {kind} target, found {targets:?}"),
        )),
    }
}

#[async_trait]
impl SessionDisk for ThinPoolDisk {
    async fn create(&self, name: &VolumeName, size_bytes: u64) -> io::Result<Volume> {
        let sectors = Self::sectors_of(size_bytes)?;
        let id = self.create_thin().await?;
        self.activate(name, id, sectors).await
    }

    async fn snapshot(&self, origin: &Volume, name: &VolumeName) -> io::Result<Volume> {
        let (thin, sectors) = self.thin_of(&origin.name).await?;
        let dm = self.dm.clone();
        let pool = self.pool.clone();
        let next_id = self.next_id.clone();
        let origin_name = self.device_name(&origin.name);
        let id = detached(async move {
            let suspended = Suspended::suspend(&dm, &origin_name).await?;
            let created = loop {
                let id = next_id.fetch_add(1, Ordering::Relaxed) & ID_MASK;
                match dm
                    .message(&pool, &format!("create_snap {id} {}", thin.id))
                    .await
                {
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    created => break created.map(|()| id),
                }
            };
            let resumed = suspended.resume().await;
            match (created, resumed) {
                (Ok(id), Ok(())) => Ok(id),
                (Ok(id), Err(e)) => {
                    // The snapshot exists but nothing will activate it.
                    if let Err(error) = dm.message(&pool, &format!("delete {id}")).await {
                        tracing::warn!(pool = %pool, id, %error, "thin left behind by a failed resume");
                    }
                    Err(e)
                }
                (Err(e), _) => Err(e),
            }
        })
        .await?;
        self.activate(name, id, sectors).await
    }

    async fn open(&self, name: &VolumeName) -> io::Result<Volume> {
        let (_, sectors) = self.thin_of(name).await?;
        self.volume(name, sectors).await
    }

    /// Waits a bounded time for the last holder to close the device.
    async fn delete(&self, name: &VolumeName) -> io::Result<()> {
        let (thin, _) = self.thin_of(name).await?;
        self.remove_device(name).await?;
        self.dm
            .message(&self.pool, &format!("delete {}", thin.id))
            .await
    }

    async fn list(&self) -> io::Result<Vec<VolumeName>> {
        Ok(self
            .thins()
            .await?
            .into_iter()
            .filter_map(|t| {
                let volume = t.name.strip_prefix(&self.pool)?.strip_prefix('.')?;
                VolumeName::new(volume.to_string()).ok()
            })
            .collect())
    }

    async fn rename(&self, from: &VolumeName, to: &VolumeName) -> io::Result<()> {
        if self.drive_kind(from).await?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{from} is a drive or a view, which is not renamed"),
            ));
        }
        self.dm
            .rename(&self.device_name(from), &self.device_name(to))
            .await?;
        tokio::fs::rename(self.node_of(from), self.node_of(to)).await
    }

    async fn capacity(&self) -> io::Result<Capacity> {
        let status = parse::pool_status(&single_target(
            &self.dm.status(&self.pool).await?,
            "thin-pool",
        )?)?;
        let data_total_bytes = status.data_total_blocks * self.block_bytes;
        let metadata_total_bytes = status.metadata_total_blocks * METADATA_BLOCK_BYTES;
        Ok(Capacity {
            data_free_bytes: data_total_bytes - status.data_used_blocks * self.block_bytes,
            data_total_bytes,
            metadata_free_bytes: metadata_total_bytes
                - status.metadata_used_blocks * METADATA_BLOCK_BYTES,
            metadata_total_bytes,
        })
    }

    /// A `clone` target over the two thins and `source`, with region
    /// [`CLONE_REGION_BYTES`].
    async fn clone_drive(
        &self,
        drive: &VolumeName,
        dest: &Volume,
        bitmap: &Volume,
        source: &Path,
        hydration: Hydration,
    ) -> io::Result<Volume> {
        let _held = self.lock_drive(drive).await;
        let device = self.device_name(drive);
        match self.dm.info(&device).await {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{drive} already exists"),
                ))
            }
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            Err(_) => {}
        }
        let (table, sectors) = self
            .clone_table(dest, bitmap, source, REGION_SECTORS)
            .await?;
        self.check_unused(&dest.name).await?;
        self.check_unused(&bitmap.name).await?;
        check_fresh_bitmap(bitmap).await?;
        self.dm
            .create(&device, sectors, "clone", &table.params(hydration))
            .await?;
        self.open_created(drive, |node| {
            Volume::open(drive.clone(), node, sectors * SECTOR_BYTES, self.chunk_size)
        })
        .await
    }

    async fn open_clone(&self, drive: &VolumeName) -> io::Result<Volume> {
        let (sectors, open): (u64, fn(_, &Path, _, _) -> _) = match self.drive(drive).await? {
            Drive::Copying { sectors, .. } | Drive::CutOver { sectors } => (sectors, Volume::open),
            Drive::View { sectors } => (sectors, Volume::open_read_only),
            Drive::Incomplete => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{drive} has no table; remove it and make it again"),
                ))
            }
        };
        let node = self.node(drive).await?;
        open(
            drive.clone(),
            &node,
            sectors * SECTOR_BYTES,
            self.chunk_size,
        )
    }

    async fn clone_state(&self, drive: &VolumeName) -> io::Result<CloneState> {
        match self.drive(drive).await? {
            Drive::Incomplete => Ok(CloneState::Incomplete),
            Drive::CutOver { .. } => Ok(CloneState::CutOver),
            Drive::Copying { .. } | Drive::View { .. } => self.clone_status(drive).await,
        }
    }

    /// One `BLKDISCARD` per joined range on the drive, then a flush. What
    /// this relies on in `drivers/md/dm-clone-target.c` (6.8 and 6.12):
    /// - A discard marks every region it covers whole as copied and
    ///   copies nothing (`process_deferred_discards`). With
    ///   `no_discard_passdown` the bio then completes without reaching
    ///   the destination (`complete_discard_bio`). Regions it covers in
    ///   part are left as they are (`bio_region_range` rounds inward).
    /// - Neither path checks whether hydration is on. With the metadata
    ///   read-only, or a failed metadata update, the discard completes
    ///   without an error and marks nothing; those regions are copied.
    /// - The marks are in memory until a commit: a flush or a suspend
    ///   commits them, as it commits copied regions. A region unmarked
    ///   by a crash is copied, which writes the same bytes again.
    /// - Without passdown the drive takes discards up to its own limit,
    ///   region-aligned, and the block layer splits a larger range at
    ///   region boundaries, so a range needs no split here.
    async fn skip_hydration(&self, drive: &VolumeName, ranges: &[Range<u64>]) -> io::Result<()> {
        let _held = self.lock_drive(drive).await;
        let (_, sectors) = self.copying_drive(drive).await?;
        let ranges = skip_ranges(ranges, sectors * SECTOR_BYTES)?;
        let node = self.node(drive).await?;
        tokio::task::spawn_blocking(move || {
            let file = File::options().write(true).open(node)?;
            for range in ranges {
                discard(&file, range.start, range.end - range.start)?;
            }
            file.sync_all()
        })
        .await
        .map_err(io::Error::other)?
    }

    async fn enable_hydration(&self, drive: &VolumeName) -> io::Result<()> {
        let _held = self.lock_drive(drive).await;
        self.copying_drive(drive).await?;
        self.dm
            .message(&self.device_name(drive), "enable_hydration")
            .await
    }

    /// Suspend, load `linear` onto the destination, resume. A region the
    /// clone has copied stays copied, so the check before the suspend
    /// still holds at the swap.
    async fn cut_over(&self, drive: &VolumeName) -> io::Result<()> {
        let (disk, drive) = (self.clone(), drive.clone());
        detached(async move {
            let _held = disk.lock_drive(&drive).await;
            disk.cut_over_locked(&drive).await
        })
        .await
    }

    /// Both snapshots, the bitmap first, are taken while the drive is
    /// suspended: the suspend stops the background copy and commits the
    /// bitmap.
    async fn snapshot_clone(
        &self,
        drive: &VolumeName,
        dest: &Volume,
        bitmap: &Volume,
        dest_snapshot: &VolumeName,
        bitmap_snapshot: &VolumeName,
    ) -> io::Result<(Volume, Volume)> {
        let disk = self.clone();
        let (drive, dest, bitmap) = (drive.clone(), dest.clone(), bitmap.clone());
        let (dest_snapshot, bitmap_snapshot) = (dest_snapshot.clone(), bitmap_snapshot.clone());
        detached(async move {
            let _held = disk.lock_drive(&drive).await;
            disk.snapshot_clone_locked(&drive, &dest, &bitmap, &dest_snapshot, &bitmap_snapshot)
                .await
        })
        .await
    }

    /// A read-only `clone` target with hydration off, with the region size
    /// the bitmap snapshot's superblock records.
    async fn clone_view(
        &self,
        view: &VolumeName,
        dest_snapshot: &Volume,
        bitmap_snapshot: &Volume,
        source: &Path,
    ) -> io::Result<Volume> {
        let _held = self.lock_drive(view).await;
        let superblock = bitmap_snapshot.read_direct(0, CLONE_HEADER_BYTES).await?;
        let region_sectors = parse::clone_superblock_region_sectors(&superblock)?;
        let (table, sectors) = self
            .clone_table(dest_snapshot, bitmap_snapshot, source, region_sectors)
            .await?;
        self.dm
            .create_read_only(
                &self.device_name(view),
                sectors,
                "clone",
                &table.params(VIEW),
            )
            .await?;
        self.open_created(view, |node| {
            Volume::open_read_only(view.clone(), node, sectors * SECTOR_BYTES, self.chunk_size)
        })
        .await
    }

    /// Also removes a drive left with no table by a crash inside
    /// `clone_drive`.
    async fn remove_clone(&self, drive: &VolumeName) -> io::Result<()> {
        let _held = self.lock_drive(drive).await;
        self.drive(drive).await?;
        self.remove_device(drive).await
    }

    /// Reads the pool's mapping tree instead of the volumes. A block is
    /// dirty when any pool block inside it differs; `block_bytes` is a
    /// multiple of the pool's block size.
    async fn dirty_blocks(
        &self,
        before: &Volume,
        after: &Volume,
        block_bytes: u64,
    ) -> io::Result<Vec<u64>> {
        engram_host_core::disk::same_origin(before, after)?;
        engram_host_core::disk::blocks_per_chunk(before, block_bytes)?;
        let (left, _) = self.thin_of(&before.name).await?;
        let (right, _) = self.thin_of(&after.name).await?;
        self.delta(left.id, right.id, before, block_bytes).await
    }

    /// The pool blocks the snapshot maps differently from `origin`. With
    /// no origin, from an empty thin made for the read and deleted after
    /// it; one a crash leaves is deleted by the next [`Self::open`].
    async fn written_chunks(
        &self,
        origin: Option<&Volume>,
        snapshot: &Volume,
    ) -> io::Result<Vec<ChunkIndex>> {
        if let Some(origin) = origin {
            return self.dirty_chunks(origin, snapshot).await;
        }
        let (right, _) = self.thin_of(&snapshot.name).await?;
        let empty = self.create_thin().await?;
        let blocks = self
            .delta(empty, right.id, snapshot, snapshot.chunk_size)
            .await;
        if let Err(error) = self
            .dm
            .message(&self.pool, &format!("delete {empty}"))
            .await
        {
            tracing::warn!(pool = %self.pool, id = empty, %error, "empty thin left behind");
        }
        Ok(blocks?.into_iter().map(ChunkIndex).collect())
    }
}
