//! Test doubles shared by this crate's unit tests.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use engram_chunk_store::Manifest;
use engram_core::error::BlobError;
use engram_core::traits::storage::{BlobObjectMeta, ByteStream};
use engram_core::traits::BlobStorage;
use engram_fill::{Class, FillEngine};
use engram_storage_local::LocalBlobStorage;

use crate::disk::{
    Capacity, ChunkIndex, FileDisk, Hydration, SessionDisk, SourceDevice, SourceServer, Volume,
    VolumeName,
};

/// Counts the chunk GETs a stage makes; manifest reads do not count.
pub(crate) struct CountingBlob {
    inner: LocalBlobStorage,
    gets: AtomicU64,
}

impl CountingBlob {
    pub(crate) fn new(inner: LocalBlobStorage) -> Self {
        Self {
            inner,
            gets: AtomicU64::new(0),
        }
    }

    pub(crate) fn gets(&self) -> u64 {
        self.gets.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl BlobStorage for CountingBlob {
    async fn put_streaming(&self, key: &str, body: ByteStream) -> Result<u64, BlobError> {
        self.inner.put_streaming(key, body).await
    }
    async fn get_streaming(&self, key: &str) -> Result<ByteStream, BlobError> {
        if key.starts_with("chunks/") {
            self.gets.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.get_streaming(key).await
    }
    async fn head(&self, key: &str) -> Result<BlobObjectMeta, BlobError> {
        self.inner.head(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        self.inner.delete(key).await
    }
    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, BlobError> {
        self.inner.list_prefix(prefix).await
    }
}

/// A [`FileDisk`] with a hydrating device, for the lazy path without a
/// kernel: `hydrate` links `name` to the destination file, so a write
/// to either lands in both, and each `hydration` call copies one chunk
/// the plan did not skip from the source file. `settle` needs every
/// chunk copied and changes nothing: the link is direct already.
pub(crate) struct HydratingDisk {
    inner: FileDisk,
    chunk_size: u64,
    clones: Mutex<HashMap<VolumeName, Clone>>,
    /// Sources a device still copies from; a stop of one is refused.
    holders: Arc<Mutex<HashSet<PathBuf>>>,
}

struct Clone {
    source: PathBuf,
    dest: PathBuf,
    pending: Vec<ChunkIndex>,
    total: u64,
    settled: bool,
}

impl HydratingDisk {
    pub(crate) fn new(root: &Path, chunk_size: u64) -> io::Result<Self> {
        Ok(Self {
            inner: FileDisk::new(root, chunk_size)?,
            chunk_size,
            clones: Mutex::new(HashMap::new()),
            holders: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    /// A source server whose stops this disk's devices refuse while
    /// they copy.
    pub(crate) fn sources(&self, dir: &Path) -> FileSources {
        FileSources {
            dir: dir.to_path_buf(),
            holders: self.holders.clone(),
            next: AtomicU64::new(0),
            stopped: Arc::new(AtomicU64::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl SessionDisk for HydratingDisk {
    async fn create(&self, name: &VolumeName, size_bytes: u64) -> io::Result<Volume> {
        self.inner.create(name, size_bytes).await
    }
    async fn snapshot(&self, origin: &Volume, name: &VolumeName) -> io::Result<Volume> {
        self.inner.snapshot(origin, name).await
    }
    async fn open(&self, name: &VolumeName) -> io::Result<Volume> {
        self.inner.open(name).await
    }
    async fn delete(&self, name: &VolumeName) -> io::Result<()> {
        if let Some(clone) = self.clones.lock().unwrap().remove(name) {
            self.holders.lock().unwrap().remove(&clone.source);
        }
        self.inner.delete(name).await
    }
    async fn list(&self) -> io::Result<Vec<VolumeName>> {
        self.inner.list().await
    }
    async fn rename(&self, from: &VolumeName, to: &VolumeName) -> io::Result<()> {
        self.inner.rename(from, to).await
    }
    async fn capacity(&self) -> io::Result<Capacity> {
        self.inner.capacity().await
    }
    fn region_bytes(&self) -> Option<u64> {
        Some(self.chunk_size)
    }
    async fn hydrate(
        &self,
        dest: &Volume,
        source: &Path,
        name: &VolumeName,
        skip: &[ChunkIndex],
    ) -> io::Result<Volume> {
        let path = self.inner.root().join(name.as_str());
        std::fs::hard_link(&dest.path, &path)?;
        let pending = (0..dest.chunk_count())
            .map(ChunkIndex)
            .filter(|c| !skip.contains(c))
            .collect();
        self.clones.lock().unwrap().insert(
            name.clone(),
            Clone {
                source: source.to_path_buf(),
                dest: dest.path.clone(),
                pending,
                total: dest.chunk_count(),
                settled: false,
            },
        );
        self.holders.lock().unwrap().insert(source.to_path_buf());
        Volume::open(name.clone(), &path, dest.size_bytes, dest.chunk_size)
    }
    async fn hydration(&self, name: &VolumeName) -> io::Result<Hydration> {
        let mut clones = self.clones.lock().unwrap();
        let Some(clone) = clones.get_mut(name) else {
            return Ok(Hydration::Settled);
        };
        if clone.settled {
            return Ok(Hydration::Settled);
        }
        if let Some(chunk) = clone.pending.first().copied() {
            let mut bytes = vec![0u8; self.chunk_size as usize];
            File::open(&clone.source)?.read_exact_at(&mut bytes, chunk.0 * self.chunk_size)?;
            File::options()
                .write(true)
                .open(&clone.dest)?
                .write_all_at(&bytes, chunk.0 * self.chunk_size)?;
            clone.pending.remove(0);
        }
        Ok(Hydration::Hydrating {
            hydrated: clone.total - clone.pending.len() as u64,
            total: clone.total,
            source: clone.source.clone(),
        })
    }
    async fn settle(&self, name: &VolumeName, _dest: &Volume) -> io::Result<()> {
        let mut clones = self.clones.lock().unwrap();
        let clone = clones.get_mut(name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{name} is not hydrating"),
            )
        })?;
        if !clone.pending.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name}: {} chunks are not copied", clone.pending.len()),
            ));
        }
        clone.settled = true;
        self.holders.lock().unwrap().remove(&clone.source);
        Ok(())
    }
}

/// Serves a manifest as a file holding its delta chunks' bytes, written
/// from the fill engine up front; the kernel reads nothing else.
pub(crate) struct FileSources {
    dir: PathBuf,
    holders: Arc<Mutex<HashSet<PathBuf>>>,
    next: AtomicU64,
    /// Sources stopped so far.
    pub(crate) stopped: Arc<AtomicU64>,
}

struct FileSource {
    path: PathBuf,
    holders: Arc<Mutex<HashSet<PathBuf>>>,
    stopped: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl SourceDevice for FileSource {
    fn path(&self) -> &Path {
        &self.path
    }
    fn touched(&self) -> Vec<u32> {
        Vec::new()
    }
    async fn stop(self: Box<Self>) -> io::Result<()> {
        if self.holders.lock().unwrap().contains(&self.path) {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!("{} is still a device's source", self.path.display()),
            ));
        }
        std::fs::remove_file(&self.path)?;
        self.stopped.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[async_trait::async_trait]
impl SourceServer for FileSources {
    async fn serve(
        &self,
        fill: FillEngine,
        manifest: &Manifest,
        delta: Vec<u64>,
        _region_bytes: u64,
    ) -> io::Result<Box<dyn SourceDevice>> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self
            .dir
            .join(format!("src-{}", self.next.fetch_add(1, Ordering::Relaxed)));
        let file = File::create_new(&path)?;
        file.set_len(manifest.total_bytes)?;
        let chunk_size = manifest.chunk_size.as_u64();
        for chunk in manifest
            .chunks
            .iter()
            .filter(|c| delta.contains(&(c.offset / chunk_size)))
        {
            let bytes = fill
                .read(chunk.hash, Class::Hydrate)
                .await
                .map_err(io::Error::other)?;
            file.write_all_at(&bytes, chunk.offset)?;
        }
        Ok(Box::new(FileSource {
            path,
            holders: self.holders.clone(),
            stopped: self.stopped.clone(),
        }))
    }

    async fn recover(
        &self,
        path: &Path,
        _fill: FillEngine,
        _manifest: &Manifest,
        _delta: Vec<u64>,
        _region_bytes: u64,
    ) -> io::Result<Box<dyn SourceDevice>> {
        std::fs::metadata(path)?;
        Ok(Box::new(FileSource {
            path: path.to_path_buf(),
            holders: self.holders.clone(),
            stopped: self.stopped.clone(),
        }))
    }
}
