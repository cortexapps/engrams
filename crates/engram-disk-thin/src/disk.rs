//! The disk over the ioctl layer; Linux only.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::dm;
use crate::parse::{self, DevNo, ThinParams, METADATA_BLOCK_BYTES};
use async_trait::async_trait;
use engram_host_core::disk::{
    punch_hole, Capacity, ChunkIndex, Hydration, SessionDisk, Volume, VolumeName,
};

const SECTOR_BYTES: u64 = 512;
/// dm-thin device ids are 24 bits wide.
const ID_MASK: u32 = 0x00FF_FFFF;
/// A `delete` waits this long in all for the last holder of a device
/// to close it.
const REMOVE_ATTEMPTS: u32 = 10;
const REMOVE_RETRY: Duration = Duration::from_millis(100);
/// A dm-clone metadata volume: a superblock and one bit per region in
/// 4 KiB blocks. 8 MiB holds the bitset of a 2 million region volume
/// with room for its transaction shadows.
const CLONE_METADATA_BYTES: u64 = 8 << 20;
/// Regions per background copy and copies in flight: 1 MiB bios, 8 MiB
/// in flight. The kernel's defaults of 1 and 1 copy at a few MiB/s.
const HYDRATION_BATCH_REGIONS: u64 = 16;
const HYDRATION_THRESHOLD_REGIONS: u64 = 128;

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
        // A host-agent that died inside `while_suspended` left a device
        // holding every I/O; its inactive table, if any, is the one it
        // meant to make live.
        let prefix = format!("{pool}.");
        for (device, _) in disk.dm.list().await? {
            if device.starts_with(&prefix) && disk.dm.suspended(&device).await? {
                tracing::warn!(device, "resuming a device left suspended");
                disk.dm.resume(&device).await?;
            }
        }
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

    /// The one target of the device behind `name`.
    async fn target_of(&self, name: &VolumeName) -> io::Result<dm::Target> {
        match self.dm.table(&self.device_name(name)).await?.as_slice() {
            [target] => Ok(target.clone()),
            targets => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{name} has {} targets, not one", targets.len()),
            )),
        }
    }

    /// The thin behind `name` and its size, refused when it belongs to
    /// another pool.
    async fn thin_of(&self, name: &VolumeName) -> io::Result<(ThinParams, u64)> {
        let target = self.target_of(name).await?;
        if target.kind != "thin" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} is a {} device, not a thin", target.kind),
            ));
        }
        let params = parse::thin_params(&target.params)?;
        if params.pool != self.pool_dev {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{name} is a thin of pool {}, not {}",
                    params.pool, self.pool
                ),
            ));
        }
        Ok((params, target.length_sectors))
    }

    /// Run `op` while `device` is suspended, then resume it, as one owned
    /// task: a suspended device holds every I/O, so the resume runs
    /// whatever happens to the caller. Both outcomes come back; what a
    /// failed resume costs is the caller's to decide.
    async fn while_suspended<T, Fut>(
        &self,
        device: String,
        op: impl FnOnce(dm::Dm) -> Fut + Send + 'static,
    ) -> io::Result<(io::Result<T>, io::Result<()>)>
    where
        T: Send + 'static,
        Fut: std::future::Future<Output = io::Result<T>> + Send + 'static,
    {
        let dm = self.dm.clone();
        tokio::spawn(async move {
            dm.suspend(&device).await?;
            let done = op(dm.clone()).await;
            let resumed = dm.resume(&device).await;
            Ok((done, resumed))
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Remove the device, waiting a bounded time for the last holder to
    /// close it, then its node.
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

    async fn delete_thin(&self, name: &VolumeName) -> io::Result<()> {
        let (thin, _) = self.thin_of(name).await?;
        self.remove_device(name).await?;
        self.dm
            .message(&self.pool, &format!("delete {}", thin.id))
            .await
    }

    /// Delete the metadata thin of the hydrating device `name`, if a
    /// settle or an earlier delete has not.
    async fn delete_metadata(&self, name: &VolumeName) -> io::Result<()> {
        match self.delete_thin(&metadata_name(name)?).await {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            deleted => deleted,
        }
    }

    /// A device over thin `id`. On failure the thin is deleted from the
    /// pool again, so a failed create or snapshot leaves nothing.
    async fn activate(&self, name: &VolumeName, id: u32, sectors: u64) -> io::Result<Volume> {
        let params = format!("{} {id}", self.pool_dev);
        let activated = async {
            self.dm
                .create(&self.device_name(name), sectors, "thin", &params)
                .await?;
            match self.volume(name, sectors).await {
                Ok(volume) => Ok(volume),
                Err(e) => {
                    let _ = tokio::fs::remove_file(self.node_of(name)).await;
                    let _ = self.dm.remove(&self.device_name(name)).await;
                    Err(e)
                }
            }
        }
        .await;
        if activated.is_err() {
            if let Err(error) = self.dm.message(&self.pool, &format!("delete {id}")).await {
                tracing::warn!(pool = %self.pool, id, %error, "thin left behind by a failed activate");
            }
        }
        activated
    }

    async fn volume(&self, name: &VolumeName, sectors: u64) -> io::Result<Volume> {
        let node = self.node_of(name);
        dm::make_node(&node, self.dm.devno(&self.device_name(name)).await?)?;
        Volume::open(name.clone(), &node, sectors * SECTOR_BYTES, self.chunk_size)
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

/// The metadata volume of a hydrating device.
fn metadata_name(name: &VolumeName) -> io::Result<VolumeName> {
    VolumeName::new(format!("{name}-md"))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

/// Mark the `skip` chunks of a hydrating device as copied: under
/// `no_discard_passdown`, a discard of regions the kernel has not copied
/// marks them copied and leaves the destination's blocks as they are.
/// Consecutive chunks go in one discard; a skip list is most of a
/// volume, in long runs.
async fn skip_regions(device: &Volume, skip: &[ChunkIndex]) -> io::Result<()> {
    let file = File::options().read(true).write(true).open(&device.path)?;
    let (chunk, size) = (device.chunk_size, device.size_bytes);
    let mut chunks: Vec<u64> = skip.iter().map(|c| c.0).collect();
    chunks.sort_unstable();
    chunks.dedup();
    tokio::task::spawn_blocking(move || {
        let mut chunks = chunks.into_iter().peekable();
        while let Some(first) = chunks.next() {
            let mut last = first;
            while chunks.next_if_eq(&(last + 1)).is_some() {
                last += 1;
            }
            let offset = first * chunk;
            let end = (last + 1) * chunk;
            if end > size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("skip chunk {last} lies beyond the {size}-byte volume"),
                ));
            }
            punch_hole(&file, offset, end - offset)?;
        }
        Ok(())
    })
    .await
    .map_err(io::Error::other)?
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
        let pool = self.pool.clone();
        let next_id = self.next_id.clone();
        let (created, resumed) = self
            .while_suspended(self.device_name(&origin.name), move |dm| async move {
                loop {
                    let id = next_id.fetch_add(1, Ordering::Relaxed) & ID_MASK;
                    match dm
                        .message(&pool, &format!("create_snap {id} {}", thin.id))
                        .await
                    {
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                        created => return created.map(|()| id),
                    }
                }
            })
            .await?;
        let id = match (created, resumed) {
            (Ok(id), Ok(())) => id,
            (Ok(id), Err(e)) => {
                // The snapshot exists but nothing will activate it.
                if let Err(error) = self.dm.message(&self.pool, &format!("delete {id}")).await {
                    tracing::warn!(pool = %self.pool, id, %error, "thin left behind by a failed resume");
                }
                return Err(e);
            }
            (Err(e), _) => return Err(e),
        };
        self.activate(name, id, sectors).await
    }

    async fn open(&self, name: &VolumeName) -> io::Result<Volume> {
        let (_, sectors) = self.thin_of(name).await?;
        self.volume(name, sectors).await
    }

    /// Waits a bounded time for the last holder to close the device. A
    /// hydrating or settled device takes its metadata thin with it.
    async fn delete(&self, name: &VolumeName) -> io::Result<()> {
        let target = self.target_of(name).await?;
        match target.kind.as_str() {
            "thin" => self.delete_thin(name).await,
            "clone" | "linear" => {
                self.remove_device(name).await?;
                self.delete_metadata(name).await
            }
            kind => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} is a {kind} device, which this disk did not make"),
            )),
        }
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

    /// A `clone` target over `dest` with `source` as its source and a
    /// region per pool block (the unit the pool shares and `dirty_blocks`
    /// sees; a guest write to a cold region waits for one region's copy),
    /// created with hydration off so the skip discards land before any
    /// copy; then hydration on. A failure leaves nothing.
    async fn hydrate(
        &self,
        dest: &Volume,
        source: &Path,
        name: &VolumeName,
        skip: &[ChunkIndex],
    ) -> io::Result<Option<Volume>> {
        let (_, sectors) = self.thin_of(&dest.name).await?;
        let region_sectors = self.block_bytes / SECTOR_BYTES;
        if !sectors.is_multiple_of(region_sectors) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}: {sectors} sectors is not whole pool blocks", dest.name),
            ));
        }
        let metadata = self
            .create(&metadata_name(name)?, CLONE_METADATA_BYTES)
            .await?;
        let params = format!(
            "{} {} {} {region_sectors} 2 no_hydration no_discard_passdown \
             4 hydration_threshold {HYDRATION_THRESHOLD_REGIONS} \
             hydration_batch_size {HYDRATION_BATCH_REGIONS}",
            dm::devno_of(&metadata.path)?,
            dm::devno_of(&dest.path)?,
            dm::devno_of(source)?,
        );
        let device = self.device_name(name);
        let hydrating = async {
            self.dm.create(&device, sectors, "clone", &params).await?;
            let volume = self.volume(name, sectors).await?;
            skip_regions(&volume, skip).await?;
            self.dm.message(&device, "enable_hydration").await?;
            Ok(volume)
        }
        .await;
        if hydrating.is_err() {
            let removed = match self.remove_device(name).await {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                removed => removed,
            };
            if let Err(error) = removed.and(self.delete_metadata(name).await) {
                tracing::warn!(%name, %error, "hydrating device left behind by a failed hydrate");
            }
        }
        hydrating.map(Some)
    }

    async fn hydration(&self, name: &VolumeName) -> io::Result<Hydration> {
        let status = self.dm.status(&self.device_name(name)).await?;
        match status.as_slice() {
            [target] if target.kind == "clone" => {
                let status = parse::clone_status(&target.params)?;
                Ok(Hydration::Hydrating {
                    hydrated: status.hydrated_regions,
                    total: status.total_regions,
                })
            }
            [target] if target.kind == "linear" => Ok(Hydration::Settled),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} is not a hydrating device: {status:?}"),
            )),
        }
    }

    /// Reloads the device as `linear` over `dest` under a suspend. A
    /// failed reload resumes the clone table. The metadata thin goes
    /// last: the kernel closes it when the clone table is destroyed.
    async fn settle(&self, name: &VolumeName, dest: &Volume) -> io::Result<()> {
        match self.hydration(name).await? {
            Hydration::Hydrating { hydrated, total } if hydrated < total => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{name}: {hydrated} of {total} regions are hydrated"),
                ));
            }
            Hydration::Hydrating { .. } => {
                let (_, sectors) = self.thin_of(&dest.name).await?;
                let params = format!("{} 0", dm::devno_of(&dest.path)?);
                let device = self.device_name(name);
                let (loaded, resumed) = self
                    .while_suspended(device.clone(), move |dm| async move {
                        dm.load(&device, sectors, "linear", &params).await
                    })
                    .await?;
                loaded.and(resumed)?;
            }
            Hydration::Settled => {}
        }
        self.delete_metadata(name).await
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
