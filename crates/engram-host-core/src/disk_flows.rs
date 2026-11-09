//! Chunks between a [`Volume`] and the chunk store: [`stage`] turns a
//! manifest into a volume, [`hydration_plan`] says what a kernel copy
//! does instead, [`checkpoint`] turns a volume into a manifest.

use std::collections::{BTreeMap, HashMap};
use std::io;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use engram_chunk_store::{
    ChunkHash, ChunkRef, ChunkSize, ChunkStore, Manifest, ManifestDelta, ManifestKind,
};
use engram_fill::{Class, FillEngine};

use crate::disk::{blocks_per_chunk, is_zero_chunk, ChunkIndex, SessionDisk, Volume, VolumeName};
use tracing::Instrument as _;

/// Store calls in flight; peak buffered memory is this many chunks.
const CONCURRENCY: usize = 64;

/// Chunks fetched but not yet written while a volume stages. The disk
/// writes more slowly than the store delivers, so this bounds memory.
const STAGE_BUFFERED: usize = 64;

fn store_err(e: engram_chunk_store::ChunkStoreError) -> io::Error {
    io::Error::other(e)
}

pub(crate) fn shape_matches(volume: &Volume, manifest: &Manifest) -> io::Result<()> {
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

/// The class a stage fetches at. It is read again for each chunk, so a
/// caller that starts to wait for a stage in flight can raise it.
pub trait StageClass: Send + Sync {
    fn class(&self) -> Class;
}

impl StageClass for Class {
    fn class(&self) -> Class {
        *self
    }
}

/// Writes `delta` into `volume`. Records on the current span how many
/// written chunks the chunk cache held (`cache`) and how many were
/// fetched (`fetched`: from a peer or the store).
async fn apply_delta(
    volume: &Volume,
    fill: &FillEngine,
    class: &dyn StageClass,
    delta: &ManifestDelta,
) -> io::Result<()> {
    let chunk_size = volume.chunk_size;
    let buffered = Arc::new(tokio::sync::Semaphore::new(STAGE_BUFFERED));
    let cache = fill.store().chunk_cache();
    let fetched = Arc::new(AtomicU64::new(0));
    let mut tasks = tokio::task::JoinSet::new();
    for c in &delta.write {
        let slot = Arc::clone(&buffered)
            .acquire_owned()
            .await
            .expect("never closed");
        let class = class.class();
        let (fill, volume) = (fill.clone(), volume.clone());
        let (hash, index) = (c.hash, ChunkIndex(c.offset / chunk_size));
        if !cache.is_some_and(|cache| cache.contains(hash)) {
            fetched.fetch_add(1, Ordering::Relaxed);
        }
        // One task per chunk, so fetch, hash and write use every core.
        // `read` keeps a miss out of the chunk cache: the volume holds it.
        // The write takes a disk-write slot, so it goes ahead of the bulk
        // cache writes of a lower class on the same disk.
        tasks.spawn(async move {
            let _slot = slot;
            let bytes = fill.read(hash, class).await.map_err(store_err)?;
            let _write = fill.disk_write(class).await;
            volume.write_chunk(index, bytes).await
        });
        while let Some(done) = tasks.try_join_next() {
            done.map_err(io::Error::other)??;
        }
    }
    while let Some(done) = tasks.join_next().await {
        done.map_err(io::Error::other)??;
    }
    let fetched = fetched.load(Ordering::Relaxed);
    let span = tracing::Span::current();
    span.record("cache", delta.write.len() as u64 - fetched);
    span.record("fetched", fetched);
    async {
        for offset in &delta.zero {
            volume.zero_chunk(ChunkIndex(offset / chunk_size)).await?;
        }
        Ok::<_, io::Error>(())
    }
    .instrument(tracing::info_span!(
        "disk.stage.zero",
        chunks = delta.zero.len()
    ))
    .await
}

/// A new volume holding `manifest`. With `base` (a volume and the
/// manifest it holds) the result is a snapshot of it with only the delta
/// written; without, a fresh volume with every listed chunk written.
/// Missing chunks are fetched at `class`.
pub async fn stage(
    disk: &dyn SessionDisk,
    fill: &FillEngine,
    class: Class,
    name: &VolumeName,
    manifest: &Manifest,
    base: Option<(&Volume, &Manifest)>,
) -> io::Result<Volume> {
    stage_at(disk, fill, &class, name, manifest, base).await
}

/// [`stage`] at a class that can change while it runs.
pub async fn stage_at(
    disk: &dyn SessionDisk,
    fill: &FillEngine,
    class: &dyn StageClass,
    name: &VolumeName,
    manifest: &Manifest,
    base: Option<(&Volume, &Manifest)>,
) -> io::Result<Volume> {
    manifest.validate().map_err(store_err)?;
    let (volume, delta) = match base {
        Some((base_volume, base_manifest)) => {
            shape_matches(base_volume, base_manifest)?;
            let delta = manifest.delta_from(base_manifest).map_err(store_err)?;
            let volume = disk
                .snapshot(base_volume, name)
                .instrument(tracing::info_span!("disk.stage.base_snapshot"))
                .await?;
            (volume, delta)
        }
        None => {
            let volume = disk
                .create(name, manifest.total_bytes)
                .instrument(tracing::info_span!("disk.stage.create"))
                .await?;
            let delta = ManifestDelta {
                write: manifest.chunks.clone(),
                zero: Vec::new(),
            };
            (volume, delta)
        }
    };
    shape_matches(&volume, manifest)?;
    let bytes: u64 = delta
        .write
        .iter()
        .map(|c| volume.chunk_size.min(volume.size_bytes - c.offset))
        .sum();
    // `base_held`: listed chunks the base volume already holds.
    let delta_span = tracing::info_span!(
        "disk.stage.delta",
        chunks = delta.write.len(),
        bytes,
        base_held = manifest.chunks.len().saturating_sub(delta.write.len()),
        cache = tracing::field::Empty,
        fetched = tracing::field::Empty,
    );
    apply_delta(&volume, fill, class, &delta)
        .instrument(delta_span)
        .await?;
    volume
        .sync()
        .instrument(tracing::info_span!("disk.stage.sync"))
        .await?;
    Ok(volume)
}

/// What brings a snapshot of `base`'s volume (an empty volume when
/// `None`) to `wanted` when the kernel copies the difference in behind
/// a running guest ([`SessionDisk::hydrate`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HydrationPlan {
    /// The chunks the kernel copies from the source, ascending: listed
    /// in `wanted` with a hash the base does not hold at that offset.
    pub delta: Vec<u64>,
    /// Chunks to zero on the destination before the copy starts: the
    /// base holds them, `wanted` does not.
    pub zero: Vec<ChunkIndex>,
    /// Every chunk not in `delta`: the destination holds it already.
    pub skip: Vec<ChunkIndex>,
}

pub fn hydration_plan(wanted: &Manifest, base: Option<&Manifest>) -> io::Result<HydrationPlan> {
    wanted.validate().map_err(store_err)?;
    let chunk_size = wanted.chunk_size.as_u64();
    let (write, zero) = match base {
        Some(base) => {
            let delta = wanted.delta_from(base).map_err(store_err)?;
            (delta.write, delta.zero)
        }
        None => (wanted.chunks.clone(), Vec::new()),
    };
    let mut delta: Vec<u64> = write.iter().map(|c| c.offset / chunk_size).collect();
    delta.sort_unstable();
    let skip = (0..wanted.expected_chunk_count())
        .filter(|chunk| delta.binary_search(chunk).is_err())
        .map(ChunkIndex)
        .collect();
    Ok(HydrationPlan {
        delta,
        zero: zero
            .iter()
            .map(|offset| ChunkIndex(offset / chunk_size))
            .collect(),
        skip,
    })
}

/// The chunks of `first_touch` (guest reads of a hydrating volume, in
/// order) that `delta` copies, as `wanted`'s hashes: what a restore
/// fetches at `Prefix` before the copy reaches them. Chunks the base
/// already holds and chunks `wanted` leaves out need no fetch.
pub(crate) fn prefix_hashes(
    first_touch: &[u32],
    delta: &[u64],
    wanted: &Manifest,
) -> Vec<ChunkHash> {
    let chunk_size = wanted.chunk_size.as_u64();
    let by_offset: HashMap<u64, ChunkHash> =
        wanted.chunks.iter().map(|c| (c.offset, c.hash)).collect();
    first_touch
        .iter()
        .map(|i| u64::from(*i))
        .filter(|i| delta.binary_search(i).is_ok())
        .filter_map(|i| by_offset.get(&(i * chunk_size)).copied())
        .collect()
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

        let chunks = indices.into_iter().map(|index| {
            let snapshot = snapshot.clone();
            (index, async move { snapshot.read_chunk(index).await })
        });
        let delta = put_chunks(store, snapshot.chunk_size, chunks).await?;
        Ok(base_manifest.apply(&delta))
    }

    /// Outside the pause, for a snapshot of a hydrating volume. The
    /// volume logically holds `wanted`; a block whose mapping still
    /// equals `base`'s is one the copy has not reached, and holds the
    /// base's bytes where `wanted`'s belong. So a block whose mapping
    /// changed since `base` (hydrated or written) is read from the
    /// snapshot and any other block is `wanted`'s: a chunk with no
    /// changed block keeps `wanted`'s entry unread, one changed
    /// throughout is read whole, and a mixed one is `wanted`'s chunk
    /// with the changed blocks laid over it. `block_bytes` is the
    /// mapping unit; it divides the chunk size.
    pub async fn upload_hydrating(
        disk: &dyn SessionDisk,
        fill: &FillEngine,
        snapshot: &Volume,
        base: &Volume,
        wanted: &Manifest,
        block_bytes: u64,
    ) -> io::Result<Manifest> {
        shape_matches(snapshot, wanted)?;
        let per_chunk = blocks_per_chunk(snapshot, block_bytes)?;
        let chunk_size = snapshot.chunk_size;
        let mut changed: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for block in disk.dirty_blocks(base, snapshot, block_bytes).await? {
            changed
                .entry(block / per_chunk)
                .or_default()
                .push((block % per_chunk) * block_bytes);
        }
        let wanted_hashes: HashMap<u64, ChunkHash> =
            wanted.chunks.iter().map(|c| (c.offset, c.hash)).collect();
        let chunks = changed.into_iter().map(|(chunk, blocks)| {
            let index = ChunkIndex(chunk);
            let offset = chunk * chunk_size;
            let (snapshot, fill) = (snapshot.clone(), fill.clone());
            let hash = wanted_hashes.get(&offset).copied();
            (index, async move {
                if blocks.len() as u64 == per_chunk {
                    return snapshot.read_chunk(index).await;
                }
                let mut bytes = match hash {
                    Some(hash) => fill
                        .read(hash, Class::Hydrate)
                        .await
                        .map_err(store_err)?
                        .to_vec(),
                    None => vec![0; snapshot.read_chunk(index).await?.len()],
                };
                for at in blocks {
                    let end = (at + block_bytes).min(bytes.len() as u64);
                    let part = snapshot.read_range(offset + at, end - at).await?;
                    bytes[at as usize..end as usize].copy_from_slice(&part);
                }
                Ok(bytes)
            })
        });
        let delta = put_chunks(fill.store(), chunk_size, chunks).await?;
        Ok(wanted.apply(&delta))
    }

    /// Put the chunks `chunks` yields, each a chunk index and the future
    /// of its bytes, and return the delta they make: a chunk of zeros is
    /// a hole. One task per chunk, so the 16 MiB read, the zero check,
    /// the SHA-256 and the PUT's TLS spread over every core.
    async fn put_chunks<F>(
        store: &ChunkStore,
        chunk_size: u64,
        chunks: impl Iterator<Item = (ChunkIndex, F)>,
    ) -> io::Result<ManifestDelta>
    where
        F: std::future::Future<Output = io::Result<Vec<u8>>> + Send + 'static,
    {
        let mut tasks = tokio::task::JoinSet::new();
        let mut results: Vec<(u64, Option<ChunkRef>)> = Vec::new();
        for (index, bytes) in chunks {
            while tasks.len() >= CONCURRENCY {
                let done = tasks.join_next().await.expect("tasks is not empty");
                results.push(done.map_err(io::Error::other)??);
            }
            let store = store.clone();
            tasks.spawn(async move {
                let offset = index.0 * chunk_size;
                let bytes = bytes.await?;
                if is_zero_chunk(&bytes) {
                    return Ok::<_, io::Error>((offset, None));
                }
                let hash = store
                    .put_chunk_bytes(bytes.into())
                    .await
                    .map_err(store_err)?;
                Ok((offset, Some(ChunkRef { offset, hash })))
            });
        }
        while let Some(done) = tasks.join_next().await {
            results.push(done.map_err(io::Error::other)??);
        }
        let mut delta = ManifestDelta::default();
        for (offset, written) in results {
            match written {
                Some(c) => delta.write.push(c),
                None => delta.zero.push(offset),
            }
        }
        Ok(delta)
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

        let staged = stage(
            &r.disk,
            &engram_fill::FillEngine::new(r.store.clone(), engram_fill::Config::default()),
            Class::Demand,
            &name("staged"),
            &manifest,
            None,
        )
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
        let fresh = stage(
            &r.disk,
            &engram_fill::FillEngine::new(r.store.clone(), engram_fill::Config::default()),
            Class::Demand,
            &name("fresh"),
            &m2,
            None,
        )
        .await
        .unwrap();
        let base = stage(
            &r.disk,
            &engram_fill::FillEngine::new(r.store.clone(), engram_fill::Config::default()),
            Class::Demand,
            &name("base"),
            &m1,
            None,
        )
        .await
        .unwrap();
        let over = stage(
            &r.disk,
            &engram_fill::FillEngine::new(r.store.clone(), engram_fill::Config::default()),
            Class::Demand,
            &name("over"),
            &m2,
            Some((&base, &m1)),
        )
        .await
        .unwrap();
        assert_eq!(bytes_of(&fresh).await, bytes_of(&c2).await);
        assert_eq!(bytes_of(&over).await, bytes_of(&c2).await);
    }

    /// Base [1, 2, 3, 0, 5]; wanted [1, 9, 0, 4, 5]: chunk 1 changed,
    /// chunk 2 dropped, chunk 3 new, the rest held.
    #[test]
    fn a_hydration_plan_copies_the_changed_chunks_zeroes_the_dropped_and_skips_the_rest() {
        let manifest = |pattern: &[u8]| Manifest {
            chunk_size: ChunkSize::bytes(CHUNK),
            chunks: pattern
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte != 0)
                .map(|(i, byte)| ChunkRef {
                    offset: i as u64 * CHUNK,
                    hash: engram_chunk_store::ChunkHash::of(&fill(*byte)),
                })
                .collect(),
            ..Manifest::empty(ManifestKind::Disk, CHUNK * pattern.len() as u64)
        };
        let base = manifest(&[1, 2, 3, 0, 5]);
        let wanted = manifest(&[1, 9, 0, 4, 5]);
        assert_eq!(
            hydration_plan(&wanted, Some(&base)).unwrap(),
            HydrationPlan {
                delta: vec![1, 3],
                zero: vec![ChunkIndex(2)],
                skip: [0, 2, 4].map(ChunkIndex).to_vec(),
            }
        );
        assert_eq!(
            hydration_plan(&wanted, None).unwrap(),
            HydrationPlan {
                delta: vec![0, 1, 3, 4],
                zero: vec![],
                skip: vec![ChunkIndex(2)],
            }
        );
        let other_shape = manifest(&[1, 2]);
        assert!(hydration_plan(&wanted, Some(&other_shape)).is_err());
    }

    /// Base [1, 2, 3, 0]; wanted [7, 8, 3, 9]. The snapshot of the
    /// hydrating volume holds chunk 0 hydrated (7), the first half of
    /// chunk 1 written (5) with the rest not yet copied (still 2), chunk
    /// 2 held, and chunk 3 not yet copied (still 0). The published disk
    /// is the wanted one with the write laid over it.
    #[tokio::test]
    async fn a_checkpoint_of_a_hydrating_volume_lays_the_changed_blocks_over_the_wanted_disk() {
        let r = rig();
        let fill_engine = engram_fill::FillEngine::new(r.store.clone(), Default::default());
        let base = r.disk.create(&name("base"), CHUNK * 4).await.unwrap();
        for (i, byte) in [1u8, 2, 3].into_iter().enumerate() {
            base.write_chunk(ChunkIndex(i as u64), fill(byte))
                .await
                .unwrap();
        }
        let mut wanted_chunks = Vec::new();
        for (i, byte) in [7u8, 8, 3, 9].into_iter().enumerate() {
            wanted_chunks.push(ChunkRef {
                offset: i as u64 * CHUNK,
                hash: r.store.put_chunk(&fill(byte)).await.unwrap(),
            });
        }
        let wanted = Manifest {
            chunk_size: ChunkSize::bytes(CHUNK),
            chunks: wanted_chunks,
            ..Manifest::empty(ManifestKind::Disk, CHUNK * 4)
        };
        let snapshot = r.disk.snapshot(&base, &name("snap")).await.unwrap();
        snapshot.write_chunk(ChunkIndex(0), fill(7)).await.unwrap();
        snapshot
            .write_range(CHUNK, vec![5u8; (CHUNK / 2) as usize])
            .await
            .unwrap();

        let published = checkpoint_split::upload_hydrating(
            &r.disk,
            &fill_engine,
            &snapshot,
            &base,
            &wanted,
            CHUNK / 2,
        )
        .await
        .unwrap();
        let mut chunk_1 = vec![5u8; (CHUNK / 2) as usize];
        chunk_1.extend(vec![8u8; (CHUNK / 2) as usize]);
        let expected = [fill(7), chunk_1, fill(3), fill(9)];
        assert_eq!(published.chunks.len(), 4);
        for (i, bytes) in expected.iter().enumerate() {
            assert_eq!(
                published.chunks[i],
                ChunkRef {
                    offset: i as u64 * CHUNK,
                    hash: ChunkHash::of(bytes)
                },
                "chunk {i}"
            );
        }
        assert_eq!(
            fill_engine
                .read(published.chunks[1].hash, Class::Hydrate)
                .await
                .unwrap()
                .to_vec(),
            expected[1]
        );
    }

    /// Reads of chunks 3, 0 and 4 came first; the copy brings 1 and 3,
    /// so only chunk 3 is fetched ahead.
    #[test]
    fn the_prefix_is_the_first_touch_in_order_restricted_to_the_delta() {
        let wanted = Manifest {
            chunk_size: ChunkSize::bytes(CHUNK),
            chunks: [1u8, 9, 0, 4, 5]
                .iter()
                .enumerate()
                .filter(|(_, b)| **b != 0)
                .map(|(i, b)| ChunkRef {
                    offset: i as u64 * CHUNK,
                    hash: ChunkHash::of(&fill(*b)),
                })
                .collect(),
            ..Manifest::empty(ManifestKind::Disk, CHUNK * 5)
        };
        assert_eq!(
            prefix_hashes(&[3, 0, 4], &[1, 3], &wanted),
            vec![ChunkHash::of(&fill(4))]
        );
        assert!(prefix_hashes(&[], &[1, 3], &wanted).is_empty());
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
            &engram_fill::FillEngine::new(r.store.clone(), engram_fill::Config::default()),
            Class::Demand,
            &name("x"),
            &m_bigger,
            Some((&base, &m_base)),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("shapes differ"), "{err}");
    }
}
