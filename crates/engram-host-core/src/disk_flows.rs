//! Chunks between a [`Volume`] and the chunk store: [`stage`] turns a
//! manifest into a volume, [`checkpoint`] turns a volume into a manifest.

use std::io;

use engram_chunk_store::{ChunkRef, ChunkSize, ChunkStore, Manifest, ManifestDelta, ManifestKind};
use futures::stream::{self, TryStreamExt};
use tracing::Instrument as _;

use crate::disk::{is_zero_chunk, ChunkIndex, SessionDisk, Volume, VolumeName};

/// Store calls in flight; peak buffered memory is this many chunks.
const CONCURRENCY: usize = 64;

/// Data-plane duration logs only; never a decision input.
#[allow(clippy::disallowed_methods)] // measurement timer, not a simulation input (ADR 0098 D1)
fn metrics_now() -> std::time::Instant {
    std::time::Instant::now()
}

/// `ENGRAM_PREFETCH_SPAWN=1` also runs the disk delta on one task per
/// chunk (default off). Read once.
fn spawn_delta() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| matches!(std::env::var("ENGRAM_PREFETCH_SPAWN").as_deref(), Ok("1")))
}

fn store_err(e: engram_chunk_store::ChunkStoreError) -> io::Error {
    io::Error::other(e)
}

fn shape_matches(volume: &Volume, manifest: &Manifest) -> io::Result<()> {
    if volume.chunk_size != manifest.chunk_size.as_u64()
        || volume.size_bytes != manifest.total_bytes
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "volume {} is {} bytes in {} byte chunks; manifest is {} bytes in {} byte chunks",
                volume.name,
                volume.size_bytes,
                volume.chunk_size,
                manifest.total_bytes,
                manifest.chunk_size.as_u64()
            ),
        ));
    }
    Ok(())
}

async fn apply_delta(volume: &Volume, store: &ChunkStore, delta: &ManifestDelta) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    let chunk_size = volume.chunk_size;
    let started = metrics_now();
    // Summed over concurrent tasks; the fetch includes the store's hash check.
    let fetch_us = AtomicU64::new(0);
    let write_us = AtomicU64::new(0);
    let (fetch_us, write_us) = (&fetch_us, &write_us);
    if spawn_delta() {
        // One task per chunk so fetch, hash and write spread over the
        // runtime's workers; at most CONCURRENCY in flight.
        let volume = volume.clone();
        let fetch_total = std::sync::Arc::new(AtomicU64::new(0));
        let write_us_total = std::sync::Arc::new(AtomicU64::new(0));
        let mut set = tokio::task::JoinSet::new();
        let mut chunks = delta.write.iter().cloned();
        let spawn = |set: &mut tokio::task::JoinSet<io::Result<()>>, c: ChunkRef| {
            let (store, volume) = (store.clone(), volume.clone());
            let (f_us, w_us) = (fetch_total.clone(), write_us_total.clone());
            set.spawn(async move {
                let t = metrics_now();
                // The volume keeps these bytes, so a miss is not stored in
                // the chunk cache: that would double the disk writes.
                let bytes = store.read_chunk(c.hash).await.map_err(store_err)?;
                f_us.fetch_add(t.elapsed().as_micros() as u64, Relaxed);
                let t = metrics_now();
                let written = volume
                    .write_chunk(ChunkIndex(c.offset / chunk_size), bytes)
                    .await;
                w_us.fetch_add(t.elapsed().as_micros() as u64, Relaxed);
                written
            });
        };
        for c in chunks.by_ref().take(CONCURRENCY) {
            spawn(&mut set, c);
        }
        while let Some(joined) = set.join_next().await {
            joined.map_err(io::Error::other)??;
            if let Some(c) = chunks.next() {
                spawn(&mut set, c);
            }
        }
        fetch_us.fetch_add(fetch_total.load(Relaxed), Relaxed);
        write_us.fetch_add(write_us_total.load(Relaxed), Relaxed);
    } else {
        stream::iter(delta.write.iter().map(Ok))
            .try_for_each_concurrent(CONCURRENCY, |c| async move {
                let t = metrics_now();
                let bytes = store.read_chunk(c.hash).await.map_err(store_err)?;
                fetch_us.fetch_add(t.elapsed().as_micros() as u64, Relaxed);
                let t = metrics_now();
                let written = volume
                    .write_chunk(ChunkIndex(c.offset / chunk_size), bytes)
                    .await;
                write_us.fetch_add(t.elapsed().as_micros() as u64, Relaxed);
                written
            })
            .await?;
    }
    let fetch_write_elapsed_ms = started.elapsed().as_millis() as u64;
    let t = metrics_now();
    if spawn_delta() {
        stream::iter(delta.zero.iter().map(Ok))
            .try_for_each_concurrent(CONCURRENCY, |offset| {
                volume.zero_chunk(ChunkIndex(offset / chunk_size))
            })
            .await?;
    } else {
        for offset in &delta.zero {
            volume.zero_chunk(ChunkIndex(offset / chunk_size)).await?;
        }
    }
    tracing::info!(
        volume = %volume.name,
        write_chunks = delta.write.len(),
        write_bytes = delta.write.len() as u64 * chunk_size,
        zero_chunks = delta.zero.len(),
        fetch_write_elapsed_ms,
        fetch_ms_sum = fetch_us.load(Relaxed) / 1000,
        write_ms_sum = write_us.load(Relaxed) / 1000,
        zero_ms = t.elapsed().as_millis() as u64,
        concurrency = CONCURRENCY,
        "disk delta applied"
    );
    Ok(())
}

/// A new volume holding `manifest`. With `base` (a volume and the
/// manifest it holds) the result is a snapshot of it with only the delta
/// written; without, a fresh volume with every listed chunk written.
pub async fn stage(
    disk: &dyn SessionDisk,
    store: &ChunkStore,
    name: &VolumeName,
    manifest: &Manifest,
    base: Option<(&Volume, &Manifest)>,
) -> io::Result<Volume> {
    manifest.validate().map_err(store_err)?;
    let (volume, delta) = match base {
        Some((base_volume, base_manifest)) => {
            shape_matches(base_volume, base_manifest)?;
            let delta = manifest.delta_from(base_manifest).map_err(store_err)?;
            let snap = disk
                .snapshot(base_volume, name)
                .instrument(tracing::info_span!("disk.thin_snapshot"))
                .await?;
            (snap, delta)
        }
        None => {
            let volume = disk
                .create(name, manifest.total_bytes)
                .instrument(tracing::info_span!("disk.create"))
                .await?;
            let delta = ManifestDelta {
                write: manifest.chunks.clone(),
                zero: Vec::new(),
            };
            (volume, delta)
        }
    };
    shape_matches(&volume, manifest)?;
    apply_delta(&volume, store, &delta)
        .instrument(tracing::info_span!(
            "disk.delta_fetch_write",
            chunks = delta.write.len(),
            zero = delta.zero.len()
        ))
        .await?;
    volume
        .sync()
        .instrument(tracing::info_span!("disk.sync"))
        .await?;
    Ok(volume)
}

/// A snapshot of `live` and the manifest describing it. With `previous`
/// (the last snapshot and its manifest) only the chunks that differ are
/// uploaded. The guest must be paused across [`checkpoint_split::take`];
/// use the split form to resume it before the upload.
pub async fn checkpoint(
    disk: &dyn SessionDisk,
    store: &ChunkStore,
    live: &Volume,
    name: &VolumeName,
    previous: Option<(&Volume, &Manifest)>,
) -> io::Result<(Volume, Manifest)> {
    let snapshot = checkpoint_split::take(disk, live, name).await?;
    let manifest = checkpoint_split::upload(disk, store, &snapshot, previous).await?;
    Ok((snapshot, manifest))
}

/// [`checkpoint`] in two halves.
pub mod checkpoint_split {
    use super::*;

    /// Inside the guest pause: sync, then snapshot.
    pub async fn take(
        disk: &dyn SessionDisk,
        live: &Volume,
        name: &VolumeName,
    ) -> io::Result<Volume> {
        live.sync().await?;
        disk.snapshot(live, name).await
    }

    /// Outside the pause: upload what changed and build the manifest.
    pub async fn upload(
        disk: &dyn SessionDisk,
        store: &ChunkStore,
        snapshot: &Volume,
        previous: Option<(&Volume, &Manifest)>,
    ) -> io::Result<Manifest> {
        let (indices, base_manifest) = match previous {
            Some((prev_volume, prev_manifest)) => {
                shape_matches(prev_volume, prev_manifest)?;
                (
                    disk.dirty_chunks(prev_volume, snapshot).await?,
                    prev_manifest.clone(),
                )
            }
            None => {
                let all = (0..snapshot.chunk_count()).map(ChunkIndex).collect();
                let empty = Manifest {
                    chunk_size: ChunkSize::bytes(snapshot.chunk_size),
                    ..Manifest::empty(ManifestKind::Disk, snapshot.size_bytes)
                };
                (all, empty)
            }
        };

        let chunk_size = snapshot.chunk_size;
        let one = |index: ChunkIndex, snapshot: Volume, store: ChunkStore| async move {
            let offset = index.0 * chunk_size;
            let bytes = snapshot.read_chunk(index).await?;
            if is_zero_chunk(&bytes) {
                return Ok::<_, io::Error>((offset, None));
            }
            let hash = store.put_chunk(&bytes).await.map_err(store_err)?;
            Ok((offset, Some(ChunkRef { offset, hash })))
        };
        let results: Vec<(u64, Option<ChunkRef>)> = if spawn_delta() {
            // One task per chunk: read, hash, encrypt and PUT spread over
            // the runtime's workers; at most CONCURRENCY in flight.
            let mut out = Vec::with_capacity(indices.len());
            let mut set = tokio::task::JoinSet::new();
            let mut rest = indices.into_iter();
            for index in rest.by_ref().take(CONCURRENCY) {
                set.spawn(one(index, snapshot.clone(), store.clone()));
            }
            while let Some(joined) = set.join_next().await {
                out.push(joined.map_err(io::Error::other)??);
                if let Some(index) = rest.next() {
                    set.spawn(one(index, snapshot.clone(), store.clone()));
                }
            }
            out
        } else {
            stream::iter(indices.into_iter().map(Ok))
                .map_ok(|index| one(index, snapshot.clone(), store.clone()))
                .try_buffer_unordered(CONCURRENCY)
                .try_collect()
                .await?
        };

        let mut delta = ManifestDelta::default();
        for (offset, written) in results {
            match written {
                Some(c) => delta.write.push(c),
                None => delta.zero.push(offset),
            }
        }
        Ok(base_manifest.apply(&delta))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use engram_chunk_store::ChunkHash;
    use engram_storage_local::LocalBlobStorage;

    use super::*;
    use crate::disk::FileDisk;

    const CHUNK: u64 = 4096;

    struct Rig {
        _dir: tempfile::TempDir,
        disk: FileDisk,
        store: ChunkStore,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let disk = FileDisk::new(dir.path().join("volumes"), CHUNK).unwrap();
        let store = ChunkStore::new(Arc::new(LocalBlobStorage::new(dir.path().join("blobs"))));
        Rig {
            _dir: dir,
            disk,
            store,
        }
    }

    fn name(s: &str) -> VolumeName {
        VolumeName::new(s).unwrap()
    }

    fn fill(byte: u8) -> Vec<u8> {
        vec![byte; CHUNK as usize]
    }

    async fn bytes_of(v: &Volume) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for i in 0..v.chunk_count() {
            out.push(v.read_chunk(ChunkIndex(i)).await.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn a_first_checkpoint_uploads_every_nonzero_chunk_and_stage_reproduces_it() {
        let r = rig();
        let live = r.disk.create(&name("live"), CHUNK * 4).await.unwrap();
        live.write_chunk(ChunkIndex(1), fill(1)).await.unwrap();
        live.write_chunk(ChunkIndex(3), fill(3)).await.unwrap();

        let (snap, manifest) = checkpoint(&r.disk, &r.store, &live, &name("c1"), None)
            .await
            .unwrap();
        assert_eq!(manifest.total_bytes, CHUNK * 4);
        assert_eq!(
            manifest.chunks.iter().map(|c| c.offset).collect::<Vec<_>>(),
            vec![CHUNK, 3 * CHUNK]
        );
        assert_eq!(manifest.chunks[0].hash, ChunkHash::of(&fill(1)));

        let staged = stage(&r.disk, &r.store, &name("staged"), &manifest, None)
            .await
            .unwrap();
        assert_eq!(bytes_of(&staged).await, bytes_of(&snap).await);
    }

    #[tokio::test]
    async fn a_later_checkpoint_uploads_only_what_changed_and_drops_zeroed_chunks() {
        let r = rig();
        let live = r.disk.create(&name("live"), CHUNK * 4).await.unwrap();
        live.write_chunk(ChunkIndex(0), fill(1)).await.unwrap();
        live.write_chunk(ChunkIndex(1), fill(2)).await.unwrap();
        let (c1, m1) = checkpoint(&r.disk, &r.store, &live, &name("c1"), None)
            .await
            .unwrap();

        // Rewrite chunk 1, zero chunk 0, write chunk 2; chunk 3 stays a hole.
        live.write_chunk(ChunkIndex(1), fill(9)).await.unwrap();
        live.zero_chunk(ChunkIndex(0)).await.unwrap();
        live.write_chunk(ChunkIndex(2), fill(5)).await.unwrap();
        let (c2, m2) = checkpoint(&r.disk, &r.store, &live, &name("c2"), Some((&c1, &m1)))
            .await
            .unwrap();

        let delta = m2.delta_from(&m1).unwrap();
        assert_eq!(
            delta.write.iter().map(|c| c.offset).collect::<Vec<_>>(),
            vec![CHUNK, 2 * CHUNK]
        );
        assert_eq!(delta.zero, vec![0]);

        // Staging m2 fresh, or over a staged m1, both reproduce the snapshot.
        let fresh = stage(&r.disk, &r.store, &name("fresh"), &m2, None)
            .await
            .unwrap();
        let base = stage(&r.disk, &r.store, &name("base"), &m1, None)
            .await
            .unwrap();
        let over = stage(&r.disk, &r.store, &name("over"), &m2, Some((&base, &m1)))
            .await
            .unwrap();
        assert_eq!(bytes_of(&fresh).await, bytes_of(&c2).await);
        assert_eq!(bytes_of(&over).await, bytes_of(&c2).await);
    }

    #[tokio::test]
    async fn stage_refuses_a_manifest_whose_shape_differs_from_its_base() {
        let r = rig();
        let base = r.disk.create(&name("base"), CHUNK * 2).await.unwrap();
        let m_base = Manifest {
            chunk_size: ChunkSize::bytes(CHUNK),
            ..Manifest::empty(ManifestKind::Disk, CHUNK * 2)
        };
        let m_bigger = Manifest {
            chunk_size: ChunkSize::bytes(CHUNK),
            ..Manifest::empty(ManifestKind::Disk, CHUNK * 3)
        };
        let err = stage(
            &r.disk,
            &r.store,
            &name("x"),
            &m_bigger,
            Some((&base, &m_base)),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("shapes differ"), "{err}");
    }
}
