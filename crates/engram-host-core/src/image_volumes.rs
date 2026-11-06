//! Image versions as volumes. The desired set comes from the
//! coordinator, the disk's volume list is the actual state, and one
//! pass converges the second toward the first. A volume
//! named `img-<manifest>-v<N>` exists only once its stage finished,
//! because a stage writes under `tmp-img-...` and renames.
//!
//! A sandbox created from an image forks the image's volume, staging it
//! first when the host has none. One lock per image makes that stage
//! and a pass's stage of the same image one stage, and keeps a pass
//! from deleting a volume while a fork snapshots it.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex};

use engram_chunk_store::{ChunkStore, Manifest};
use engram_core::types::manifest::ManifestRef;
use engram_fill::{Class, FillEngine};
use uuid::Uuid;

use crate::disk::{SessionDisk, Volume, VolumeName};
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

fn parse_image(rest: &str) -> Option<ManifestRef> {
    let (id, version) = rest.split_once("-v")?;
    Some(ManifestRef {
        manifest_id: Uuid::try_parse(id).ok()?,
        version: version.parse().ok()?,
    })
}

/// The image a finished volume holds; `None` for every other volume.
pub fn image_of(name: &VolumeName) -> Option<ManifestRef> {
    parse_image(name.as_str().strip_prefix(IMAGE_PREFIX)?)
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

/// The image volumes of one disk.
pub struct ImageVolumes {
    disk: Arc<dyn SessionDisk>,
    fill: FillEngine,
    locks: Mutex<HashMap<ManifestRef, Arc<tokio::sync::Mutex<()>>>>,
}

impl ImageVolumes {
    pub fn new(disk: Arc<dyn SessionDisk>, fill: FillEngine) -> Self {
        Self {
            disk,
            fill,
            locks: Mutex::new(HashMap::new()),
        }
    }

    fn lock_of(&self, image: ManifestRef) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lock().unwrap().entry(image).or_default().clone()
    }

    /// One pass: delete unfinished stages, stage desired images that have
    /// no volume, then delete image volumes that are no longer desired. A
    /// new image stages as a snapshot of the present volume it shares the
    /// most chunks with, which is why the undesired volumes go last. A
    /// failure on one volume is recorded and the pass continues. `Err`
    /// only when the disk cannot be listed.
    pub async fn reconcile(&self, desired: &[ManifestRef]) -> io::Result<Report> {
        let disk = self.disk.as_ref();
        let mut report = Report::default();
        let mut present = Vec::new();
        for name in disk.list().await? {
            if let Some(rest) = name.as_str().strip_prefix(STAGING_PREFIX) {
                // A stage in flight holds its image's lock.
                let lock = parse_image(rest).map(|image| self.lock_of(image));
                let Ok(_held) = lock.as_ref().map(|l| l.try_lock()).transpose() else {
                    continue;
                };
                match disk.delete(&name).await {
                    Ok(()) => report.deleted.push(name),
                    Err(e) => report.failed.push((name, e)),
                }
            } else if let Some(image) = image_of(&name) {
                present.push(image);
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
            let lock = self.lock_of(image);
            let _held = lock.lock().await;
            let result = match disk.open(&volume_name(image)).await {
                Ok(_) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    stage_one(disk, &self.fill, Class::Background, image, &present).await
                }
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => present.push(image),
                Err(e) => report.failed.push((volume_name(image), e)),
            }
        }

        for image in present.clone() {
            if desired.contains(&image) {
                continue;
            }
            let name = volume_name(image);
            let lock = self.lock_of(image);
            let _held = lock.lock().await;
            match disk.delete(&name).await {
                Ok(()) => {
                    present.retain(|p| *p != image);
                    report.deleted.push(name);
                }
                Err(e) => report.failed.push((name, e)),
            }
        }

        self.locks
            .lock()
            .unwrap()
            .retain(|_, lock| Arc::strong_count(lock) > 1);
        report.staged = desired
            .iter()
            .copied()
            .filter(|image| present.contains(image))
            .collect();
        Ok(report)
    }

    /// Snapshot the volume of `image` as `dest`. When this host has no
    /// volume of the image, stage it first, from the present volume it
    /// shares the most chunks with. The volume stays for the next pass,
    /// which deletes it when the image is not desired.
    pub async fn fork(&self, image: ManifestRef, dest: &VolumeName) -> io::Result<Volume> {
        let disk = self.disk.as_ref();
        let lock = self.lock_of(image);
        let _held = lock.lock().await;
        let origin = match disk.open(&volume_name(image)).await {
            Ok(origin) => origin,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let present = staged(&disk.list().await?);
                stage_one(disk, &self.fill, Class::Demand, image, &present).await?;
                disk.open(&volume_name(image)).await?
            }
            Err(e) => return Err(e),
        };
        disk.snapshot(&origin, dest).await
    }
}

/// Stage `image` under its staging name and rename it. The caller holds
/// the image's lock, so a staging volume already on the disk is the
/// remains of a stage that did not finish.
async fn stage_one(
    disk: &dyn SessionDisk,
    fill: &FillEngine,
    class: Class,
    image: ManifestRef,
    present: &[ManifestRef],
) -> io::Result<()> {
    let store = fill.store();
    let manifest = store.get_manifest(image).await.map_err(io::Error::other)?;
    let base = closest(disk, store, &manifest, present).await;
    let staging = staging_name(image);
    match disk.delete(&staging).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let staged = stage(
        disk,
        fill,
        class,
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

/// The present volume whose manifest holds the most of `wanted`'s
/// chunks at the same offsets, when one holds any. A volume whose
/// manifest the store no longer has, or whose shape differs, is not a
/// candidate.
async fn closest(
    disk: &dyn SessionDisk,
    store: &ChunkStore,
    wanted: &Manifest,
    present: &[ManifestRef],
) -> Option<(Volume, Manifest)> {
    let wanted_chunks: HashSet<(u64, _)> =
        wanted.chunks.iter().map(|c| (c.offset, c.hash)).collect();
    let mut best: Option<(usize, Volume, Manifest)> = None;
    for candidate in present {
        let Ok(manifest) = store.get_manifest(*candidate).await else {
            continue;
        };
        if manifest.chunk_size != wanted.chunk_size || manifest.total_bytes != wanted.total_bytes {
            continue;
        }
        let shared = manifest
            .chunks
            .iter()
            .filter(|c| wanted_chunks.contains(&(c.offset, c.hash)))
            .count();
        if shared == 0 || best.as_ref().is_some_and(|(most, _, _)| *most >= shared) {
            continue;
        }
        let Ok(volume) = disk.open(&volume_name(*candidate)).await else {
            continue;
        };
        best = Some((shared, volume, manifest));
    }
    best.map(|(_, volume, manifest)| (volume, manifest))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use engram_chunk_store::Manifest;
    use engram_storage_local::LocalBlobStorage;

    use crate::test_support::CountingBlob;

    use super::*;
    use crate::disk::{ChunkIndex, FileDisk, Volume};
    use crate::disk_flows::checkpoint;

    const CHUNK: u64 = 4096;

    struct Rig {
        _dir: tempfile::TempDir,
        disk: Arc<FileDisk>,
        store: ChunkStore,
        blob: Arc<CountingBlob>,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let blob = Arc::new(CountingBlob::new(LocalBlobStorage::new(
            dir.path().join("blobs"),
        )));
        Rig {
            disk: Arc::new(FileDisk::new(dir.path().join("volumes"), CHUNK).unwrap()),
            store: ChunkStore::new(blob.clone()),
            blob,
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
        let (_, manifest) = checkpoint(r.disk.as_ref(), &r.store, &v, &snap, None)
            .await
            .unwrap();
        r.store.put_manifest(image, &manifest).await.unwrap();
        r.disk.delete(&scratch).await.unwrap();
        r.disk.delete(&snap).await.unwrap();
        manifest
    }

    fn images(r: &Rig) -> ImageVolumes {
        ImageVolumes::new(
            r.disk.clone(),
            engram_fill::FillEngine::new(r.store.clone(), engram_fill::Config::default()),
        )
    }

    async fn pass(r: &Rig, desired: &[ManifestRef]) -> Report {
        images(r).reconcile(desired).await.unwrap()
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

        let report = pass(&r, &[image]).await;
        assert_eq!(report.staged, vec![image]);
        assert!(report.failed.is_empty());
        assert_eq!(names(&r.disk).await, vec![volume_name(image).as_str()]);
        let v = r.disk.open(&volume_name(image)).await.unwrap();
        assert_eq!(bytes_of(&v).await, vec![1, 0, 3]);

        let again = pass(&r, &[image]).await;
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

        pass(&r, &[v1]).await;
        let report = pass(&r, &[v1, v2]).await;
        assert_eq!(report.staged, vec![v1, v2]);
        let one = r.disk.open(&volume_name(v1)).await.unwrap();
        let two = r.disk.open(&volume_name(v2)).await.unwrap();
        assert_eq!(bytes_of(&one).await, vec![1, 2, 3]);
        assert_eq!(bytes_of(&two).await, vec![1, 0, 9]);
    }

    /// A re-bake of an image is a new lineage; it stages as a snapshot
    /// of the volume it shares the most chunks with and fetches only
    /// the difference, and the old volume goes only after that.
    #[tokio::test]
    async fn a_new_lineage_stages_from_the_present_volume_it_shares_most_with() {
        let r = rig();
        let old = ManifestRef::new();
        let other = ManifestRef::new();
        let rebaked = ManifestRef::new();
        publish(&r, old, &[1, 2, 3, 4]).await;
        publish(&r, other, &[1, 0, 0, 0]).await;
        publish(&r, rebaked, &[1, 2, 3, 9]).await;
        pass(&r, &[old, other]).await;

        let gets = r.blob.gets();
        let report = pass(&r, &[other, rebaked]).await;
        assert_eq!(r.blob.gets(), gets + 1, "only the changed chunk is fetched");
        assert_eq!(report.deleted, vec![volume_name(old)]);
        let v = r.disk.open(&volume_name(rebaked)).await.unwrap();
        assert_eq!(bytes_of(&v).await, vec![1, 2, 3, 9]);
    }

    #[tokio::test]
    async fn undesired_and_unfinished_volumes_go_and_session_volumes_stay() {
        let r = rig();
        let old = ManifestRef::new();
        let wanted = ManifestRef::new();
        publish(&r, old, &[1]).await;
        publish(&r, wanted, &[2]).await;
        pass(&r, &[old]).await;
        r.disk.create(&staging_name(wanted), CHUNK).await.unwrap();
        let session = VolumeName::new("sb-abc-root").unwrap();
        r.disk.create(&session, CHUNK).await.unwrap();

        let report = pass(&r, &[wanted]).await;
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

        let report = pass(&r, &[missing, good]).await;
        assert_eq!(report.staged, vec![good]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, volume_name(missing));
        assert_eq!(names(&r.disk).await, vec![volume_name(good).as_str()]);
    }

    /// A sandbox from an image with no volume stages the image volume
    /// once and snapshots it; the next fork fetches nothing.
    #[tokio::test]
    async fn a_fork_stages_the_image_volume_once() {
        let r = rig();
        let image = ManifestRef::new();
        publish(&r, image, &[4, 0, 6]).await;
        let images = images(&r);

        let first = VolumeName::new("sb-a-root").unwrap();
        let v = images.fork(image, &first).await.unwrap();
        assert_eq!(bytes_of(&v).await, vec![4, 0, 6]);
        let gets = r.blob.gets();
        let second = VolumeName::new("sb-b-root").unwrap();
        images.fork(image, &second).await.unwrap();
        assert_eq!(r.blob.gets(), gets, "the second fork is a snapshot");
        assert_eq!(
            names(&r.disk).await,
            vec![volume_name(image).as_str(), "sb-a-root", "sb-b-root"]
        );
    }

    /// The capture of a new image forks its first version; the captured
    /// version then stages as a delta over it and the first goes.
    #[tokio::test]
    async fn the_captured_version_stages_over_the_forked_one() {
        let r = rig();
        let v1 = ManifestRef::new();
        let v2 = v1.next_version();
        publish(&r, v1, &[1, 2, 3, 4]).await;
        publish(&r, v2, &[1, 2, 3, 7]).await;
        let images = images(&r);
        images
            .fork(v1, &VolumeName::new("sb-capture-root").unwrap())
            .await
            .unwrap();

        let gets = r.blob.gets();
        let report = images.reconcile(&[v2]).await.unwrap();
        assert_eq!(r.blob.gets(), gets + 1, "only the changed chunk is fetched");
        assert_eq!(report.deleted, vec![volume_name(v1)]);
        let v = r.disk.open(&volume_name(v2)).await.unwrap();
        assert_eq!(bytes_of(&v).await, vec![1, 2, 3, 7]);
    }

    /// A pass leaves the staging volume of a stage in flight.
    #[tokio::test]
    async fn a_pass_keeps_a_stage_in_flight() {
        let r = rig();
        let image = ManifestRef::new();
        let images = images(&r);
        r.disk.create(&staging_name(image), CHUNK).await.unwrap();

        let lock = images.lock_of(image);
        let held = lock.lock().await;
        let report = images.reconcile(&[]).await.unwrap();
        assert!(report.deleted.is_empty());
        assert_eq!(names(&r.disk).await, vec![staging_name(image).as_str()]);

        drop(held);
        let report = images.reconcile(&[]).await.unwrap();
        assert_eq!(report.deleted, vec![staging_name(image)]);
    }
}
