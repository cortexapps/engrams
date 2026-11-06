//! Chunks between a [`Volume`] and the chunk store: [`stage`] turns a
//! manifest into a volume, [`checkpoint`] turns a volume into a manifest.

use std::io;

use engram_chunk_store::{
    ChunkHash, ChunkRef, ChunkSize, ChunkStore, Manifest, ManifestDelta, ManifestKind,
};
use futures::stream::{self, TryStreamExt};

use crate::disk::{is_zero_chunk, ChunkIndex, SessionDisk, Volume, VolumeName};

/// Store calls in flight; peak buffered memory is this many chunks.
const CONCURRENCY: usize = 64;

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

async fn apply_delta(volume: &Volume, store: &ChunkStore, delta: &ManifestDelta) -> io::Result<()> {
    let chunk_size = volume.chunk_size;
    stream::iter(delta.write.iter().map(Ok))
        .try_for_each_concurrent(CONCURRENCY, |c| async move {
            let bytes = store.get_chunk(c.hash).await.map_err(store_err)?;
            volume
                .write_chunk(ChunkIndex(c.offset / chunk_size), bytes.to_vec())
                .await
        })
        .await?;
    for offset in &delta.zero {
        volume.zero_chunk(ChunkIndex(offset / chunk_size)).await?;
    }
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
            (disk.snapshot(base_volume, name).await?, delta)
        }
        None => {
            let volume = disk.create(name, manifest.total_bytes).await?;
            let delta = ManifestDelta {
                write: manifest.chunks.clone(),
                zero: Vec::new(),
            };
            (volume, delta)
        }
    };
    shape_matches(&volume, manifest)?;
    apply_delta(&volume, store, &delta).await?;
    volume.sync().await?;
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

        upload_over(store, snapshot, indices, &base_manifest).await
    }

    /// `base` with the chunks at `indices` replaced by what `from` holds
    /// there. A chunk equal to the one `base` holds at its offset is not
    /// uploaded; an all-zero chunk becomes a hole.
    pub async fn upload_over(
        store: &ChunkStore,
        from: &Volume,
        indices: Vec<ChunkIndex>,
        base: &Manifest,
    ) -> io::Result<Manifest> {
        shape_matches(from, base)?;
        let chunk_size = from.chunk_size;
        let results: Vec<(u64, Option<ChunkRef>)> = stream::iter(indices.into_iter().map(Ok))
            .map_ok(|index| async move {
                let offset = index.0 * chunk_size;
                let bytes = from.read_chunk(index).await?;
                if is_zero_chunk(&bytes) {
                    return Ok::<_, io::Error>((offset, None));
                }
                let hash = ChunkHash::of(&bytes);
                if base.chunk_at(offset).is_none_or(|c| c.hash != hash) {
                    store.put_chunk(&bytes).await.map_err(store_err)?;
                }
                Ok((offset, Some(ChunkRef { offset, hash })))
            })
            .try_buffer_unordered(CONCURRENCY)
            .try_collect()
            .await?;

        let mut delta = ManifestDelta::default();
        for (offset, written) in results {
            match written {
                Some(c) => delta.write.push(c),
                None => delta.zero.push(offset),
            }
        }
        Ok(base.apply(&delta))
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
