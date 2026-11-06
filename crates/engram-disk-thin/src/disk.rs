//! The disk over the ioctl layer; Linux only.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::dm;
use crate::parse::{self, CloneTable, DevNo, ThinParams, METADATA_BLOCK_BYTES};
use async_trait::async_trait;
use engram_host_core::disk::{
    Capacity, CloneState, Hydration, SessionDisk, Volume, VolumeName, CLONE_REGION_BYTES,
};

const SECTOR_BYTES: u64 = 512;
const REGION_SECTORS: u64 = CLONE_REGION_BYTES / SECTOR_BYTES;
/// A view copies nothing, so its tuning is never used.
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
    metadata_snap: tokio::sync::Mutex<()>,
}

impl ThinPoolDisk {
    /// Attach to an active pool whose block size divides `chunk_size`,
    /// delete thins that have no device (a crash between `delete` and
    /// the pool message), and seed the id counter. Device nodes are made
    /// under `nodes_dir`.
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
            metadata_snap: tokio::sync::Mutex::new(()),
        };
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

    async fn drive_target(&self, drive: &VolumeName) -> io::Result<dm::Target> {
        let targets = self.dm.table(&self.device_name(drive)).await?;
        match targets.as_slice() {
            [target] if is_drive(target) => Ok(target.clone()),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{drive} is not a cloned drive: {targets:?}"),
            )),
        }
    }

    /// The clone table of a drive that has not been cut over.
    async fn copying_drive(&self, drive: &VolumeName) -> io::Result<CloneTable> {
        let target = self.drive_target(drive).await?;
        if target.kind != "clone" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{drive} is cut over"),
            ));
        }
        parse::clone_table(&target.params)
    }

    /// A clone table over two thins of this pool and `source`.
    async fn clone_table(
        &self,
        dest: &Volume,
        bitmap: &Volume,
        source: &Path,
    ) -> io::Result<(CloneTable, u64)> {
        let (_, sectors) = self.thin_of(&dest.name).await?;
        self.thin_of(&bitmap.name).await?;
        let table = CloneTable {
            metadata_dev: self.dm.devno(&self.device_name(&bitmap.name)).await?,
            dest_dev: self.dm.devno(&self.device_name(&dest.name)).await?,
            source_dev: dm::block_devno(source)?,
            region_sectors: REGION_SECTORS,
        };
        Ok((table, sectors))
    }

    /// Run `read` over a reserved metadata snapshot. The reserve is
    /// released when the guard drops, so a cancelled read releases it
    /// too. A reserve another holder left behind (a process that died,
    /// a read cancelled before this guard existed) is released first.
    async fn with_metadata_snap<T>(
        &self,
        read: impl std::future::Future<Output = io::Result<T>>,
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

/// A suspended device; resumed on drop if [`Self::resume`] did not run.
struct Suspended<'a> {
    dm: &'a dm::Dm,
    name: &'a str,
    held: bool,
}

impl<'a> Suspended<'a> {
    async fn suspend(dm: &'a dm::Dm, name: &'a str) -> io::Result<Self> {
        dm.suspend(name).await?;
        Ok(Self {
            dm,
            name,
            held: true,
        })
    }

    async fn resume(mut self) -> io::Result<()> {
        let resumed = self.dm.resume(self.name).await;
        self.held = false;
        resumed
    }
}

impl Drop for Suspended<'_> {
    fn drop(&mut self) {
        if self.held {
            if let Err(error) = self.dm.resume_blocking(self.name) {
                tracing::error!(device = self.name, %error, "device left suspended");
            }
        }
    }
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

/// A cloned drive's one target: `clone` while it copies, `linear` after
/// the cutover.
fn is_drive(target: &dm::Target) -> bool {
    target.kind == "clone" || target.kind == "linear"
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
        loop {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed) & ID_MASK;
            match self
                .dm
                .message(&self.pool, &format!("create_thin {id}"))
                .await
            {
                Ok(()) => return self.activate(name, id, sectors).await,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }

    async fn snapshot(&self, origin: &Volume, name: &VolumeName) -> io::Result<Volume> {
        let (thin, sectors) = self.thin_of(&origin.name).await?;
        let dm = self.dm.clone();
        let pool = self.pool.clone();
        let next_id = self.next_id.clone();
        let origin_name = self.device_name(&origin.name);
        // The origin holds all I/O while it is suspended, so the resume
        // must run whatever happens to the caller: the calls are one owned
        // task.
        let id = tokio::spawn(async move {
            dm.suspend(&origin_name).await?;
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
            let resumed = dm.resume(&origin_name).await;
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
        .await
        .map_err(io::Error::other)??;
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

    /// A `clone` target over the two thins and `source`, region 64 KiB.
    async fn clone_drive(
        &self,
        drive: &VolumeName,
        dest: &Volume,
        bitmap: &Volume,
        source: &Path,
        hydration: Hydration,
    ) -> io::Result<Volume> {
        let (table, sectors) = self.clone_table(dest, bitmap, source).await?;
        self.dm
            .create(
                &self.device_name(drive),
                sectors,
                "clone",
                &table.params(hydration),
            )
            .await?;
        self.open_created(drive, |node| {
            Volume::open(drive.clone(), node, sectors * SECTOR_BYTES, self.chunk_size)
        })
        .await
    }

    async fn open_clone(&self, drive: &VolumeName) -> io::Result<Volume> {
        let target = self.drive_target(drive).await?;
        self.volume(drive, target.length_sectors).await
    }

    async fn clone_state(&self, drive: &VolumeName) -> io::Result<CloneState> {
        let status = self.dm.status(&self.device_name(drive)).await?;
        match status.as_slice() {
            [target] if target.kind == "linear" => Ok(CloneState::CutOver),
            [target] if target.kind == "clone" => parse::clone_status(&target.params),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{drive} is not a cloned drive: {status:?}"),
            )),
        }
    }

    async fn enable_hydration(&self, drive: &VolumeName) -> io::Result<()> {
        self.copying_drive(drive).await?;
        self.dm
            .message(&self.device_name(drive), "enable_hydration")
            .await
    }

    /// Suspend, load `linear` onto the destination, resume. A region the
    /// clone has copied stays copied, so the check before the suspend
    /// still holds at the swap.
    async fn cut_over(&self, drive: &VolumeName) -> io::Result<()> {
        let target = self.drive_target(drive).await?;
        if target.kind == "linear" {
            return Ok(());
        }
        let table = parse::clone_table(&target.params)?;
        if !self.clone_state(drive).await?.is_hydrated() {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!("{drive} is still hydrating"),
            ));
        }
        let device = self.device_name(drive);
        let suspended = Suspended::suspend(&self.dm, &device).await?;
        self.dm
            .load(
                &device,
                target.length_sectors,
                "linear",
                &format!("{} 0", table.dest_dev),
            )
            .await?;
        suspended.resume().await
    }

    /// Both snapshots are taken while the drive is suspended: the suspend
    /// stops the background copy and commits the bitmap.
    async fn snapshot_clone(
        &self,
        drive: &VolumeName,
        dest: &Volume,
        bitmap: &Volume,
        dest_snapshot: &VolumeName,
        bitmap_snapshot: &VolumeName,
    ) -> io::Result<(Volume, Volume)> {
        let table = self.copying_drive(drive).await?;
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
        let device = self.device_name(drive);
        let suspended = Suspended::suspend(&self.dm, &device).await?;
        let bitmap_snap = self.snapshot(bitmap, bitmap_snapshot).await?;
        let taken = match self.snapshot(dest, dest_snapshot).await {
            Ok(dest_snap) => suspended.resume().await.map(|()| dest_snap),
            Err(e) => Err(e),
        };
        match taken {
            Ok(dest_snap) => Ok((dest_snap, bitmap_snap)),
            Err(e) => {
                drop(bitmap_snap);
                for name in [bitmap_snapshot, dest_snapshot] {
                    match self.delete(name).await {
                        Err(error) if error.kind() != io::ErrorKind::NotFound => {
                            tracing::warn!(volume = %name, %error, "snapshot left behind by a failed snapshot_clone");
                        }
                        _ => {}
                    }
                }
                Err(e)
            }
        }
    }

    /// A read-only `clone` target with hydration off.
    async fn clone_view(
        &self,
        view: &VolumeName,
        dest_snapshot: &Volume,
        bitmap_snapshot: &Volume,
        source: &Path,
    ) -> io::Result<Volume> {
        let (table, sectors) = self
            .clone_table(dest_snapshot, bitmap_snapshot, source)
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
        let targets = self.dm.table(&self.device_name(drive)).await?;
        match targets.as_slice() {
            [] => {}
            [target] if is_drive(target) => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{drive} is not a cloned drive: {targets:?}"),
                ))
            }
        }
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
        if !block_bytes.is_multiple_of(self.block_bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "block size {block_bytes} is not a multiple of the pool's {} bytes",
                    self.block_bytes
                ),
            ));
        }
        let (left, _) = self.thin_of(&before.name).await?;
        let (right, _) = self.thin_of(&after.name).await?;
        let pool_blocks = self
            .with_metadata_snap(dm::thin_delta(&self.metadata_path, left.id, right.id))
            .await?;
        let per_block = block_bytes / self.block_bytes;
        let count = engram_host_core::disk::block_count(before, block_bytes);
        let mut blocks: Vec<u64> = pool_blocks
            .into_iter()
            .map(|block| block / per_block)
            .filter(|block| *block < count)
            .collect();
        blocks.dedup();
        Ok(blocks)
    }
}
