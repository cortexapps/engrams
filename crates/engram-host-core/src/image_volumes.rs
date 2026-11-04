//! Image versions as volumes. The enabled set is the desired state, the
//! disk's volume list is the actual state, and one pass converges the
//! second toward the first. The pass keeps nothing between runs: a
//! volume named `img-<manifest>-v<N>` exists only once its stage
//! finished, because a stage writes under `tmp-img-...` and renames.

use std::io;

use engram_chunk_store::ChunkStore;
use engram_core::types::manifest::ManifestRef;
use uuid::Uuid;

use crate::disk::{SessionDisk, VolumeName};
use crate::disk_flows::stage;

const IMAGE_PREFIX: &str = "img-";
const STAGING_PREFIX: &str = "tmp-img-";

pub fn volume_name(image: ManifestRef) -> VolumeName {
    VolumeName::new(format!(
        "{IMAGE_PREFIX}{}-v{}",
        image.manifest_id.simple(),
        image.version
    ))
    .expect("an image volume name is 58 bytes of [a-z0-9-]")
}

fn staging_name(image: ManifestRef) -> VolumeName {
    VolumeName::new(format!(
        "{STAGING_PREFIX}{}-v{}",
        image.manifest_id.simple(),
        image.version
    ))
    .expect("a staging volume name is 62 bytes of [a-z0-9-]")
}

/// The image a finished volume holds; `None` for every other volume.
pub fn image_of(name: &VolumeName) -> Option<ManifestRef> {
    let rest = name.as_str().strip_prefix(IMAGE_PREFIX)?;
    let (id, version) = rest.split_once("-v")?;
    Some(ManifestRef {
        manifest_id: Uuid::try_parse(id).ok()?,
        version: version.parse().ok()?,
    })
}

/// The images whose volumes are finished, from a volume list.
pub fn staged(names: &[VolumeName]) -> Vec<ManifestRef> {
    names.iter().filter_map(image_of).collect()
}

#[derive(Debug, Default)]
pub struct Report {
    /// Desired images whose volume exists after the pass.
    pub staged: Vec<ManifestRef>,
    pub deleted: Vec<VolumeName>,
    pub failed: Vec<(VolumeName, io::Error)>,
}

/// One pass: delete unfinished stages, delete image volumes that are
/// no longer desired, stage desired images that have no volume. A
/// failure on one volume is recorded and the pass continues. `Err`
/// only when the disk cannot be listed.
pub async fn reconcile_once(
    disk: &dyn SessionDisk,
    store: &ChunkStore,
    desired: &[ManifestRef],
) -> io::Result<Report> {
    let mut report = Report::default();
    let mut present = Vec::new();
    for name in disk.list().await? {
        if name.as_str().starts_with(STAGING_PREFIX) {
            match disk.delete(&name).await {
                Ok(()) => report.deleted.push(name),
                Err(e) => report.failed.push((name, e)),
            }
        } else if let Some(image) = image_of(&name) {
            present.push(image);
        }
    }

    for image in present.clone() {
        if desired.contains(&image) {
            continue;
        }
        let name = volume_name(image);
        match disk.delete(&name).await {
            Ok(()) => {
                present.retain(|p| *p != image);
                report.deleted.push(name);
            }
            Err(e) => report.failed.push((name, e)),
        }
    }

    // Lower versions first, so a version staged in this pass can be the
    // origin of the next.
    let mut to_stage: Vec<ManifestRef> = desired
        .iter()
        .copied()
        .filter(|image| !present.contains(image))
        .collect();
    to_stage.sort_by_key(|image| (image.manifest_id, image.version));
    for image in to_stage {
        let predecessor = present
            .iter()
            .copied()
            .filter(|p| p.manifest_id == image.manifest_id && p.version < image.version)
            .max_by_key(|p| p.version);
        match stage_one(disk, store, image, predecessor).await {
            Ok(()) => present.push(image),
            Err(e) => report.failed.push((volume_name(image), e)),
        }
    }

    report.staged = desired
        .iter()
        .copied()
        .filter(|image| present.contains(image))
        .collect();
    Ok(report)
}

async fn stage_one(
    disk: &dyn SessionDisk,
    store: &ChunkStore,
    image: ManifestRef,
    predecessor: Option<ManifestRef>,
) -> io::Result<()> {
    let manifest = store.get_manifest(image).await.map_err(io::Error::other)?;
    let base = match predecessor {
        Some(p) => match (
            disk.open(&volume_name(p)).await,
            store.get_manifest(p).await,
        ) {
            (Ok(volume), Ok(manifest)) => Some((volume, manifest)),
            _ => None,
        },
        None => None,
    };
    let staging = staging_name(image);
    let staged = stage(
        disk,
        store,
        &staging,
        &manifest,
        base.as_ref().map(|(v, m)| (v, m)),
    )
    .await;
    drop(base);
    if let Err(e) = staged {
        let _ = disk.delete(&staging).await;
        return Err(e);
    }
    drop(staged);
    disk.rename(&staging, &volume_name(image)).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use engram_chunk_store::Manifest;
    use engram_storage_local::LocalBlobStorage;

    use super::*;
    use crate::disk::{ChunkIndex, FileDisk, Volume};
    use crate::disk_flows::checkpoint;

    const CHUNK: u64 = 4096;

    struct Rig {
        _dir: tempfile::TempDir,
        disk: FileDisk,
        store: ChunkStore,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        Rig {
            disk: FileDisk::new(dir.path().join("volumes"), CHUNK).unwrap(),
            store: ChunkStore::new(Arc::new(LocalBlobStorage::new(dir.path().join("blobs")))),
            _dir: dir,
        }
    }

    /// Publish an image whose chunk `i` holds byte `pattern[i]` (0 is a
    /// hole), returning its reference. The scratch volumes are removed.
    async fn publish(r: &Rig, image: ManifestRef, pattern: &[u8]) -> Manifest {
        let scratch = VolumeName::new("scratch").unwrap();
        let v = r
            .disk
            .create(&scratch, CHUNK * pattern.len() as u64)
            .await
            .unwrap();
        for (i, byte) in pattern.iter().enumerate() {
            if *byte != 0 {
                v.write_chunk(ChunkIndex(i as u64), vec![*byte; CHUNK as usize])
                    .await
                    .unwrap();
            }
        }
        let snap = VolumeName::new("scratch-snap").unwrap();
        let (_, manifest) = checkpoint(&r.disk, &r.store, &v, &snap, None)
            .await
            .unwrap();
        r.store.put_manifest(image, &manifest).await.unwrap();
        r.disk.delete(&scratch).await.unwrap();
        r.disk.delete(&snap).await.unwrap();
        manifest
    }

    async fn bytes_of(v: &Volume) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..v.chunk_count() {
            out.push(v.read_chunk(ChunkIndex(i)).await.unwrap()[0]);
        }
        out
    }

    async fn names(disk: &FileDisk) -> Vec<String> {
        let mut names: Vec<String> = disk
            .list()
            .await
            .unwrap()
            .iter()
            .map(|n| n.as_str().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn volume_names_round_trip_and_reject_other_volumes() {
        let image = ManifestRef::new();
        assert_eq!(image_of(&volume_name(image)), Some(image));
        assert_eq!(image_of(&staging_name(image)), None);
        assert_eq!(image_of(&VolumeName::new("sb-abc-root").unwrap()), None);
        assert_eq!(image_of(&VolumeName::new("img-nothex-v1").unwrap()), None);
    }

    #[tokio::test]
    async fn a_desired_image_is_staged_and_reads_as_its_manifest() {
        let r = rig();
        let image = ManifestRef::new();
        publish(&r, image, &[1, 0, 3]).await;

        let report = reconcile_once(&r.disk, &r.store, &[image]).await.unwrap();
        assert_eq!(report.staged, vec![image]);
        assert!(report.failed.is_empty());
        assert_eq!(names(&r.disk).await, vec![volume_name(image).as_str()]);
        let v = r.disk.open(&volume_name(image)).await.unwrap();
        assert_eq!(bytes_of(&v).await, vec![1, 0, 3]);

        let again = reconcile_once(&r.disk, &r.store, &[image]).await.unwrap();
        assert_eq!(again.staged, vec![image]);
        assert!(again.deleted.is_empty() && again.failed.is_empty());
    }

    #[tokio::test]
    async fn a_later_version_is_staged_over_the_earlier_one() {
        let r = rig();
        let v1 = ManifestRef::new();
        let v2 = v1.next_version();
        publish(&r, v1, &[1, 2, 3]).await;
        publish(&r, v2, &[1, 0, 9]).await;

        reconcile_once(&r.disk, &r.store, &[v1]).await.unwrap();
        let report = reconcile_once(&r.disk, &r.store, &[v1, v2]).await.unwrap();
        assert_eq!(report.staged, vec![v1, v2]);
        let one = r.disk.open(&volume_name(v1)).await.unwrap();
        let two = r.disk.open(&volume_name(v2)).await.unwrap();
        assert_eq!(bytes_of(&one).await, vec![1, 2, 3]);
        assert_eq!(bytes_of(&two).await, vec![1, 0, 9]);
    }

    #[tokio::test]
    async fn undesired_and_unfinished_volumes_go_and_session_volumes_stay() {
        let r = rig();
        let old = ManifestRef::new();
        let wanted = ManifestRef::new();
        publish(&r, old, &[1]).await;
        publish(&r, wanted, &[2]).await;
        reconcile_once(&r.disk, &r.store, &[old]).await.unwrap();
        r.disk.create(&staging_name(wanted), CHUNK).await.unwrap();
        let session = VolumeName::new("sb-abc-root").unwrap();
        r.disk.create(&session, CHUNK).await.unwrap();

        let report = reconcile_once(&r.disk, &r.store, &[wanted]).await.unwrap();
        assert_eq!(report.staged, vec![wanted]);
        let mut deleted: Vec<&str> = report.deleted.iter().map(VolumeName::as_str).collect();
        deleted.sort();
        let mut expected = [volume_name(old), staging_name(wanted)];
        expected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(
            deleted,
            expected.iter().map(VolumeName::as_str).collect::<Vec<_>>()
        );
        assert_eq!(
            names(&r.disk).await,
            vec![volume_name(wanted).as_str(), "sb-abc-root"]
        );
    }

    #[tokio::test]
    async fn one_image_without_a_manifest_does_not_block_the_others() {
        let r = rig();
        let good = ManifestRef::new();
        let missing = ManifestRef::new();
        publish(&r, good, &[5]).await;

        let report = reconcile_once(&r.disk, &r.store, &[missing, good])
            .await
            .unwrap();
        assert_eq!(report.staged, vec![good]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, volume_name(missing));
        assert_eq!(names(&r.disk).await, vec![volume_name(good).as_str()]);
    }
}
