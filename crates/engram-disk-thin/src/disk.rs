//! The disk over the ioctl layer; Linux only.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use crate::dm;
use crate::parse::{self, DevNo, ThinParams, METADATA_BLOCK_BYTES};
use async_trait::async_trait;
use engram_host_core::disk::{Capacity, ChunkIndex, SessionDisk, Volume, VolumeName};

const SECTOR_BYTES: u64 = 512;
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
    /// and `create` moves on, so a wrap of the 24-bit space is safe.
    next_id: AtomicU32,
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
            next_id: AtomicU32::new(1),
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

    fn node_of(&self, name: &VolumeName) -> PathBuf {
        self.nodes_dir.join(name.as_str())
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
        let targets = self.dm.table(name.as_str()).await?;
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
                .create(name.as_str(), sectors, "thin", &params)
                .await?;
            match self.volume(name, sectors).await {
                Ok(volume) => Ok(volume),
                Err(e) => {
                    let _ = tokio::fs::remove_file(self.node_of(name)).await;
                    let _ = self.dm.remove(name.as_str()).await;
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
        dm::make_node(&node, self.dm.devno(name.as_str()).await?)?;
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
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) & ID_MASK;
        self.dm.suspend(origin.name.as_str()).await?;
        let created = self
            .dm
            .message(&self.pool, &format!("create_snap {id} {}", thin.id))
            .await;
        self.dm.resume(origin.name.as_str()).await?;
        created?;
        self.activate(name, id, sectors).await
    }

    async fn open(&self, name: &VolumeName) -> io::Result<Volume> {
        let (_, sectors) = self.thin_of(name).await?;
        self.volume(name, sectors).await
    }

    /// Waits a bounded time for the last holder to close the device.
    async fn delete(&self, name: &VolumeName) -> io::Result<()> {
        let (thin, _) = self.thin_of(name).await?;
        let mut attempt = 1;
        loop {
            match self.dm.remove(name.as_str()).await {
                Err(e) if e.kind() == io::ErrorKind::ResourceBusy && attempt < REMOVE_ATTEMPTS => {
                    attempt += 1;
                    tokio::time::sleep(REMOVE_RETRY).await;
                }
                removed => break removed?,
            }
        }
        match tokio::fs::remove_file(self.node_of(name)).await {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        self.dm
            .message(&self.pool, &format!("delete {}", thin.id))
            .await
    }

    async fn list(&self) -> io::Result<Vec<VolumeName>> {
        Ok(self
            .thins()
            .await?
            .into_iter()
            .filter_map(|t| VolumeName::new(t.name).ok())
            .collect())
    }

    async fn rename(&self, from: &VolumeName, to: &VolumeName) -> io::Result<()> {
        self.dm.rename(from.as_str(), to.as_str()).await?;
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

    /// Reads the pool's mapping tree instead of the volumes. A chunk is
    /// dirty when any pool block inside it differs.
    async fn dirty_chunks(&self, before: &Volume, after: &Volume) -> io::Result<Vec<ChunkIndex>> {
        let ((left, left_sectors), (right, right_sectors)) = (
            self.thin_of(&before.name).await?,
            self.thin_of(&after.name).await?,
        );
        if left_sectors != right_sectors {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "dirty_chunks: {} and {} differ in size; both must be snapshots of one origin",
                    before.name, after.name
                ),
            ));
        }
        let blocks = self
            .with_metadata_snap(dm::thin_delta(&self.metadata_path, left.id, right.id))
            .await?;
        let blocks_per_chunk = self.chunk_size / self.block_bytes;
        let mut chunks: Vec<ChunkIndex> = blocks
            .into_iter()
            .map(|block| ChunkIndex(block / blocks_per_chunk))
            .filter(|chunk| chunk.0 < before.chunk_count())
            .collect();
        chunks.dedup();
        Ok(chunks)
    }
}
