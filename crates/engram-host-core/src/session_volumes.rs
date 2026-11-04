//! A session's disks as volumes. Per role (root, swap) a sandbox has a
//! live volume the hypervisor opens and one checkpoint volume. The
//! names are the only record:
//!
//! ```text
//! sb-<sandbox>-<role>                        live
//! sb-<sandbox>-<role>-ckpt-<manifest>-v<N>   the last checkpoint; holds that manifest
//! tmp-sb-<sandbox>-<role>                    a stage or a checkpoint in flight
//! ```
//!
//! A checkpoint volume is named after a manifest the chunk store holds,
//! so the name is written last: a crash leaves a `tmp-` volume, never a
//! name that lies. The manifest id in the name is the session's lineage
//! and the next checkpoint publishes under it, so no caller tracks a
//! manifest ref between calls.
//!
//! A released sandbox keeps its newest checkpoint volume per role, so a
//! resume on this host is a snapshot of it instead of a stage from blob
//! storage. [`SessionVolumes::reap`] deletes kept checkpoints, oldest
//! release first, when the pool needs the space.

use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use engram_chunk_store::{ChunkSize, ChunkStore, Manifest, ManifestKind};
use engram_core::types::ids::SandboxId;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::snapshot::DiskRole;
use uuid::Uuid;

use crate::disk::{Capacity, ChunkIndex, SessionDisk, Volume, VolumeName};
use crate::disk_flows::{checkpoint_split, stage};
use crate::image_volumes;

const LIVE_PREFIX: &str = "sb-";
const TRANSIENT_PREFIX: &str = "tmp-sb-";
const CHECKPOINT_INFIX: &str = "-ckpt-";

/// What a volume name says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Live,
    Checkpoint(ManifestRef),
    Transient,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parsed {
    pub sandbox: SandboxId,
    pub role: DiskRole,
    pub kind: Kind,
}

/// A manifest the store does not hold is `NotFound`, like a volume the
/// disk does not hold.
fn store_err(e: engram_chunk_store::ChunkStoreError) -> io::Error {
    use engram_chunk_store::ChunkStoreError;
    use engram_core::error::BlobError;
    match e {
        ChunkStoreError::Blob(BlobError::NotFound) => io::Error::new(io::ErrorKind::NotFound, e),
        e => io::Error::other(e),
    }
}

pub fn live_name(sandbox: SandboxId, role: DiskRole) -> VolumeName {
    VolumeName::new(format!(
        "{LIVE_PREFIX}{}-{}",
        sandbox.as_uuid().simple(),
        role
    ))
    .expect("a live volume name is 40 bytes of [a-z0-9-]")
}

pub fn checkpoint_name(sandbox: SandboxId, role: DiskRole, manifest: ManifestRef) -> VolumeName {
    VolumeName::new(format!(
        "{LIVE_PREFIX}{}-{}{CHECKPOINT_INFIX}{}-v{}",
        sandbox.as_uuid().simple(),
        role,
        manifest.manifest_id.simple(),
        manifest.version
    ))
    .expect("a checkpoint volume name is at most 100 bytes of [a-z0-9-]")
}

fn transient_name(sandbox: SandboxId, role: DiskRole) -> VolumeName {
    VolumeName::new(format!(
        "{TRANSIENT_PREFIX}{}-{}",
        sandbox.as_uuid().simple(),
        role
    ))
    .expect("a transient volume name is 44 bytes of [a-z0-9-]")
}

/// What `name` says; `None` for a volume that is not a session's.
pub fn parse(name: &VolumeName) -> Option<Parsed> {
    let (rest, transient) = match name.as_str().strip_prefix(TRANSIENT_PREFIX) {
        Some(rest) => (rest, true),
        None => (name.as_str().strip_prefix(LIVE_PREFIX)?, false),
    };
    let (sandbox, rest) = rest.split_once('-')?;
    let sandbox = SandboxId::from(Uuid::try_parse(sandbox).ok()?);
    let (role, rest) = match rest.split_once(CHECKPOINT_INFIX) {
        Some((role, manifest)) => (role, Some(manifest)),
        None => (rest, None),
    };
    let role: DiskRole = role.parse().ok()?;
    let kind = match (transient, rest) {
        (true, None) => Kind::Transient,
        (false, None) => Kind::Live,
        (false, Some(manifest)) => {
            let (id, version) = manifest.split_once("-v")?;
            Kind::Checkpoint(ManifestRef {
                manifest_id: Uuid::try_parse(id).ok()?,
                version: version.parse().ok()?,
            })
        }
        (true, Some(_)) => return None,
    };
    Some(Parsed {
        sandbox,
        role,
        kind,
    })
}

/// Where a live volume's contents come from. A manifest is written in
/// full before the sandbox starts; when `base` names an image staged on
/// this host, that volume is the origin and only the difference is
/// written. A manifest that is itself a staged image is snapshotted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A manifest many sandboxes share: the volume starts a new lineage
    /// that holds a copy of it.
    Fork {
        manifest: ManifestRef,
        base: Option<ManifestRef>,
    },
    /// The sandbox's own manifest: the volume continues its lineage.
    Continue {
        manifest: ManifestRef,
        base: Option<ManifestRef>,
    },
    /// Zeros, in a new lineage.
    Empty { size_bytes: u64 },
}

/// What `provision` made: the device paths a backend opens and the
/// lineage each volume checkpoints under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provisioned {
    pub root: PathBuf,
    pub root_manifest: ManifestRef,
    pub swap: Option<PathBuf>,
    pub swap_manifest: Option<ManifestRef>,
}

/// A snapshot taken under the guest pause, not yet published. Dropping
/// it leaves the transient volume for the next `take`, `pending` or
/// `release` to find.
#[derive(Debug)]
pub struct PendingCheckpoint {
    sandbox: SandboxId,
    role: DiskRole,
    snapshot: Volume,
    previous: ManifestRef,
}

impl PendingCheckpoint {
    pub fn sandbox(&self) -> SandboxId {
        self.sandbox
    }

    pub fn role(&self) -> DiskRole {
        self.role
    }

    /// The checkpoint the snapshot is a change on.
    pub fn previous(&self) -> ManifestRef {
        self.previous
    }

    /// The paused state, readable while the guest runs on.
    pub fn snapshot(&self) -> &Volume {
        &self.snapshot
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub deleted: Vec<VolumeName>,
    pub failed: Vec<(VolumeName, io::Error)>,
    /// Sandboxes that still have a checkpoint volume.
    pub kept: Vec<SandboxId>,
}

/// The owner of every `sb-` volume on a host.
pub struct SessionVolumes {
    disk: Arc<dyn SessionDisk>,
    store: ChunkStore,
    /// Released sandboxes whose checkpoint volumes stay, oldest release
    /// first. A sandbox `reconcile` finds with no live volume joins at
    /// the front: its age is unknown, so it goes first.
    kept: Mutex<VecDeque<SandboxId>>,
}

impl SessionVolumes {
    pub fn new(disk: Arc<dyn SessionDisk>, store: ChunkStore) -> Self {
        Self {
            disk,
            store,
            kept: Mutex::new(VecDeque::new()),
        }
    }

    pub fn disk(&self) -> &dyn SessionDisk {
        self.disk.as_ref()
    }

    pub async fn capacity(&self) -> io::Result<Capacity> {
        self.disk.capacity().await
    }

    /// Make the sandbox's live volumes. On any error nothing of the
    /// sandbox is left on the disk.
    pub async fn provision(
        &self,
        sandbox: SandboxId,
        root: Source,
        swap: Option<Source>,
    ) -> io::Result<Provisioned> {
        let provisioned = async {
            let (root, root_manifest) = self.provision_role(sandbox, DiskRole::Root, root).await?;
            let (swap, swap_manifest) = match swap {
                Some(source) => {
                    let (path, manifest) =
                        self.provision_role(sandbox, DiskRole::Swap, source).await?;
                    (Some(path), Some(manifest))
                }
                None => (None, None),
            };
            Ok(Provisioned {
                root,
                root_manifest,
                swap,
                swap_manifest,
            })
        }
        .await;
        if provisioned.is_err() {
            let _ = self.purge(sandbox).await;
        }
        provisioned
    }

    /// The checkpoint volume first, under the transient name until its
    /// manifest is in the store; then the live volume as its snapshot.
    /// A `Continue` of the checkpoint this host kept skips the stage;
    /// one of a newer manifest stages only the delta onto the kept
    /// checkpoint.
    async fn provision_role(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        source: Source,
    ) -> io::Result<(PathBuf, ManifestRef)> {
        let tmp = transient_name(sandbox, role);
        self.delete_if_present(&tmp).await?;
        let (checkpoint, lineage) = match source {
            Source::Fork { manifest, base } => {
                let wanted = self.store.get_manifest(manifest).await.map_err(store_err)?;
                match self.staged(manifest).await {
                    Some((origin, _)) => self.disk.snapshot(&origin, &tmp).await?,
                    None => self.stage_into(&tmp, &wanted, base).await?,
                };
                let lineage = ManifestRef::new();
                self.store
                    .put_manifest(lineage, &wanted)
                    .await
                    .map_err(store_err)?;
                (self.commit(&tmp, sandbox, role, lineage).await?, lineage)
            }
            Source::Continue { manifest, base } => match self.kept(sandbox, role).await? {
                Some((volume, _, held)) if held == manifest => (volume, manifest),
                kept => {
                    let wanted = self.store.get_manifest(manifest).await.map_err(store_err)?;
                    match kept {
                        Some((volume, held_manifest, _))
                            if volume.size_bytes == wanted.total_bytes =>
                        {
                            stage(
                                self.disk.as_ref(),
                                &self.store,
                                &tmp,
                                &wanted,
                                Some((&volume, &held_manifest)),
                            )
                            .await?
                        }
                        _ => self.stage_into(&tmp, &wanted, base).await?,
                    };
                    (self.commit(&tmp, sandbox, role, manifest).await?, manifest)
                }
            },
            Source::Empty { size_bytes } => {
                let volume = self.disk.create(&tmp, size_bytes).await?;
                let lineage = ManifestRef::new();
                let manifest = Manifest {
                    chunk_size: ChunkSize::bytes(volume.chunk_size),
                    ..Manifest::empty(ManifestKind::Disk, size_bytes)
                };
                self.store
                    .put_manifest(lineage, &manifest)
                    .await
                    .map_err(store_err)?;
                (self.commit(&tmp, sandbox, role, lineage).await?, lineage)
            }
        };
        let live = self
            .disk
            .snapshot(&checkpoint, &live_name(sandbox, role))
            .await?;
        Ok((live.path, lineage))
    }

    /// Give the staged transient volume its checkpoint name and delete
    /// the role's other checkpoints.
    async fn commit(
        &self,
        tmp: &VolumeName,
        sandbox: SandboxId,
        role: DiskRole,
        lineage: ManifestRef,
    ) -> io::Result<Volume> {
        let name = checkpoint_name(sandbox, role, lineage);
        self.disk.rename(tmp, &name).await?;
        for other in self.disk.list().await? {
            if other != name
                && parse(&other).is_some_and(|p| {
                    p.sandbox == sandbox && p.role == role && matches!(p.kind, Kind::Checkpoint(_))
                })
            {
                self.disk.delete(&other).await?;
            }
        }
        self.disk.open(&name).await
    }

    /// The newest checkpoint volume of the role, its manifest and that
    /// manifest's ref, when this host holds all three.
    async fn kept(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<Option<(Volume, Manifest, ManifestRef)>> {
        let held = match self.checkpoint_of(sandbox, role).await {
            Ok(held) => held,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let volume = self
            .disk
            .open(&checkpoint_name(sandbox, role, held))
            .await?;
        Ok(match self.store.get_manifest(held).await {
            Ok(manifest) => Some((volume, manifest, held)),
            Err(_) => None,
        })
    }

    /// Write `wanted` into a new volume `name`, from the staged volume of
    /// `base` when this host has it.
    async fn stage_into(
        &self,
        name: &VolumeName,
        wanted: &Manifest,
        base: Option<ManifestRef>,
    ) -> io::Result<Volume> {
        let base = match base {
            Some(image) => self.staged(image).await,
            None => None,
        };
        stage(
            self.disk.as_ref(),
            &self.store,
            name,
            wanted,
            base.as_ref().map(|(v, m)| (v, m)),
        )
        .await
    }

    /// The staged volume of an image and its manifest, when this host
    /// has both.
    async fn staged(&self, image: ManifestRef) -> Option<(Volume, Manifest)> {
        match (
            self.disk.open(&image_volumes::volume_name(image)).await,
            self.store.get_manifest(image).await,
        ) {
            (Ok(volume), Ok(manifest)) => Some((volume, manifest)),
            _ => None,
        }
    }

    /// The roles the sandbox has a live volume for.
    pub async fn roles(&self, sandbox: SandboxId) -> io::Result<Vec<DiskRole>> {
        let names = self.disk.list().await?;
        Ok(DiskRole::ALL
            .into_iter()
            .filter(|role| names.contains(&live_name(sandbox, *role)))
            .collect())
    }

    /// A live volume, the one the hypervisor opens.
    pub async fn live(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<Volume> {
        self.disk.open(&live_name(sandbox, role)).await
    }

    /// The device path of a live volume.
    pub async fn path(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<PathBuf> {
        Ok(self.live(sandbox, role).await?.path)
    }

    /// Under the guest pause: sync the live volume and snapshot it.
    pub async fn take(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<PendingCheckpoint> {
        let previous = self.checkpoint_of(sandbox, role).await?;
        let tmp = transient_name(sandbox, role);
        self.delete_if_present(&tmp).await?;
        let live = self.live(sandbox, role).await?;
        let snapshot = checkpoint_split::take(self.disk.as_ref(), &live, &tmp).await?;
        Ok(PendingCheckpoint {
            sandbox,
            role,
            snapshot,
            previous,
        })
    }

    /// The checkpoint a `take` left unpublished, found by name: a new
    /// process redrives the publish after a crash. `NotFound` when
    /// there is none.
    pub async fn pending(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<PendingCheckpoint> {
        let previous = self.checkpoint_of(sandbox, role).await?;
        let snapshot = self.disk.open(&transient_name(sandbox, role)).await?;
        Ok(PendingCheckpoint {
            sandbox,
            role,
            snapshot,
            previous,
        })
    }

    /// Ascending indices of the chunks the pending snapshot changed
    /// since the sandbox's checkpoint: what a live move has to carry.
    pub async fn changed(&self, pending: &PendingCheckpoint) -> io::Result<Vec<ChunkIndex>> {
        let previous = self
            .disk
            .open(&checkpoint_name(
                pending.sandbox,
                pending.role,
                pending.previous,
            ))
            .await?;
        self.disk.dirty_chunks(&previous, &pending.snapshot).await
    }

    /// Outside the pause: upload what changed since the last checkpoint,
    /// publish the manifest at the lineage's next free version, and make
    /// the snapshot the sandbox's checkpoint volume.
    pub async fn publish(&self, pending: PendingCheckpoint) -> io::Result<ManifestRef> {
        let PendingCheckpoint {
            sandbox,
            role,
            snapshot,
            previous,
        } = pending;
        let previous_name = checkpoint_name(sandbox, role, previous);
        let previous_volume = self.disk.open(&previous_name).await?;
        let previous_manifest = self.store.get_manifest(previous).await.map_err(store_err)?;
        let manifest = checkpoint_split::upload(
            self.disk.as_ref(),
            &self.store,
            &snapshot,
            Some((&previous_volume, &previous_manifest)),
        )
        .await?;
        drop(previous_volume);
        // The store, not the volume name, says which versions exist: a
        // crash after `put_manifest` and before the rename leaves a
        // version the name never reached.
        let latest = self
            .store
            .latest_manifest_version(previous.manifest_id)
            .await
            .map_err(store_err)?
            .unwrap_or(previous.version);
        let next = ManifestRef {
            manifest_id: previous.manifest_id,
            version: latest.max(previous.version) + 1,
        };
        self.store
            .put_manifest(next, &manifest)
            .await
            .map_err(store_err)?;
        let tmp = snapshot.name.clone();
        drop(snapshot);
        self.disk
            .rename(&tmp, &checkpoint_name(sandbox, role, next))
            .await?;
        self.disk.delete(&previous_name).await?;
        Ok(next)
    }

    /// [`Self::take`] then [`Self::publish`], for a guest that is paused
    /// throughout or has no guest.
    pub async fn checkpoint(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<ManifestRef> {
        let pending = self.take(sandbox, role).await?;
        self.publish(pending).await
    }

    /// Delete a pending checkpoint's snapshot without publishing it.
    pub async fn discard(&self, pending: PendingCheckpoint) -> io::Result<()> {
        let name = pending.snapshot.name.clone();
        drop(pending);
        self.disk.delete(&name).await
    }

    /// Delete the sandbox's live and transient volumes and every
    /// checkpoint volume but the newest of each role. The kept
    /// checkpoint makes a resume on this host a snapshot instead of a
    /// stage; [`Self::reap`] deletes it when the pool needs the space.
    /// Fails while a process holds a volume open.
    pub async fn release(&self, sandbox: SandboxId) -> io::Result<()> {
        let report = self.sweep(|s| s == sandbox, &[]).await?;
        if let Some((_, e)) = report.failed.into_iter().next() {
            return Err(e);
        }
        if report.kept.contains(&sandbox) {
            self.remember_kept(sandbox, false);
        }
        Ok(())
    }

    /// Delete every volume of the sandbox. A volume a process holds
    /// open stays and is reported as failed.
    pub async fn purge(&self, sandbox: SandboxId) -> io::Result<Report> {
        let mut report = Report::default();
        for name in self.disk.list().await? {
            if parse(&name).is_some_and(|p| p.sandbox == sandbox) {
                match self.disk.delete(&name).await {
                    Ok(()) => report.deleted.push(name),
                    Err(e) => report.failed.push((name, e)),
                }
            }
        }
        if report.failed.is_empty() {
            self.kept.lock().unwrap().retain(|s| *s != sandbox);
        }
        Ok(report)
    }

    /// Delete what no sandbox needs: the live and transient volumes of
    /// every sandbox not in `live` and every checkpoint volume but the
    /// newest of its role. A live sandbox keeps its transient volume:
    /// it may be a checkpoint whose publish is being redriven. Run once
    /// after the live set is known, before any sandbox is provisioned.
    pub async fn reconcile(&self, live: &[SandboxId]) -> io::Result<Report> {
        let report = self.sweep(|_| true, live).await?;
        for sandbox in &report.kept {
            if !live.contains(sandbox) {
                self.remember_kept(*sandbox, true);
            }
        }
        Ok(report)
    }

    /// Purge kept sandboxes, oldest release first, until the pool's
    /// free data space is at least `floor_bytes` or none is left. A
    /// sandbox whose purge fails moves to the back and the pass ends.
    /// A disk without a pool reports no capacity and never reaps.
    pub async fn reap(&self, floor_bytes: u64) -> io::Result<Report> {
        let mut report = Report::default();
        while self.capacity().await?.data_below(floor_bytes) {
            let next = self.kept.lock().unwrap().front().copied();
            let Some(sandbox) = next else { break };
            let purged = self.purge(sandbox).await?;
            let stuck = !purged.failed.is_empty();
            report.deleted.extend(purged.deleted);
            report.failed.extend(purged.failed);
            if stuck {
                self.kept.lock().unwrap().rotate_left(1);
                break;
            }
        }
        Ok(report)
    }

    /// Among the volumes of the sandboxes `within` selects, delete the
    /// live and transient volumes of sandboxes outside `live` and every
    /// checkpoint volume but the newest of its role.
    async fn sweep(
        &self,
        within: impl Fn(SandboxId) -> bool,
        live: &[SandboxId],
    ) -> io::Result<Report> {
        let mut report = Report::default();
        let mut newest: std::collections::HashMap<(SandboxId, DiskRole), u64> = Default::default();
        let parsed: Vec<(VolumeName, Parsed)> = self
            .disk
            .list()
            .await?
            .into_iter()
            .filter_map(|name| parse(&name).map(|p| (name, p)))
            .filter(|(_, p)| within(p.sandbox))
            .collect();
        for (_, p) in &parsed {
            if let Kind::Checkpoint(m) = p.kind {
                let v = newest.entry((p.sandbox, p.role)).or_insert(0);
                *v = (*v).max(m.version);
            }
        }
        for (name, p) in parsed {
            let unwanted = match p.kind {
                Kind::Live | Kind::Transient => !live.contains(&p.sandbox),
                Kind::Checkpoint(m) => m.version < newest[&(p.sandbox, p.role)],
            };
            if !unwanted {
                if matches!(p.kind, Kind::Checkpoint(_)) && !report.kept.contains(&p.sandbox) {
                    report.kept.push(p.sandbox);
                }
                continue;
            }
            match self.disk.delete(&name).await {
                Ok(()) => report.deleted.push(name),
                Err(e) => report.failed.push((name, e)),
            }
        }
        Ok(report)
    }

    fn remember_kept(&self, sandbox: SandboxId, age_unknown: bool) {
        let mut kept = self.kept.lock().unwrap();
        if kept.contains(&sandbox) {
            return;
        }
        match age_unknown {
            true => kept.push_front(sandbox),
            false => kept.push_back(sandbox),
        }
    }

    /// The manifest of the sandbox's checkpoint volume: what the last
    /// publish made durable.
    pub async fn checkpoint_of(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<ManifestRef> {
        let mut found: Option<ManifestRef> = None;
        for name in self.disk.list().await? {
            if let Some(Parsed {
                sandbox: s,
                role: r,
                kind: Kind::Checkpoint(m),
            }) = parse(&name)
            {
                if s == sandbox && r == role && found.is_none_or(|f| m.version > f.version) {
                    found = Some(m);
                }
            }
        }
        found.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("sandbox {sandbox} has no {} checkpoint volume", role),
            )
        })
    }

    async fn delete_if_present(&self, name: &VolumeName) -> io::Result<()> {
        match self.disk.delete(name).await {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use engram_storage_local::LocalBlobStorage;

    use super::*;
    use crate::disk::FileDisk;
    use crate::disk_flows::checkpoint;
    use crate::test_support::CountingBlob;

    const CHUNK: u64 = 4096;

    struct Rig {
        _dir: tempfile::TempDir,
        disk: Arc<dyn SessionDisk>,
        store: ChunkStore,
        volumes: SessionVolumes,
        blob: Arc<CountingBlob>,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let disk = Arc::new(FileDisk::new(dir.path().join("volumes"), CHUNK).unwrap());
        rig_on(dir, disk)
    }

    /// A rig whose pool reports `POOL_TOTAL` bytes with `VOLUME_COST`
    /// used per volume, so reaping frees space.
    fn pressured_rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let disk = Arc::new(PressuredDisk(
            FileDisk::new(dir.path().join("volumes"), CHUNK).unwrap(),
        ));
        rig_on(dir, disk)
    }

    fn rig_on(dir: tempfile::TempDir, disk: Arc<dyn SessionDisk>) -> Rig {
        let blob = Arc::new(CountingBlob::new(LocalBlobStorage::new(
            dir.path().join("blobs"),
        )));
        let store = ChunkStore::new(blob.clone());
        Rig {
            volumes: SessionVolumes::new(disk.clone(), store.clone()),
            disk,
            store,
            blob,
            _dir: dir,
        }
    }

    const POOL_TOTAL: u64 = 100;
    const VOLUME_COST: u64 = 10;

    struct PressuredDisk(FileDisk);

    #[async_trait::async_trait]
    impl SessionDisk for PressuredDisk {
        async fn create(&self, name: &VolumeName, size_bytes: u64) -> io::Result<Volume> {
            self.0.create(name, size_bytes).await
        }
        async fn snapshot(&self, origin: &Volume, name: &VolumeName) -> io::Result<Volume> {
            self.0.snapshot(origin, name).await
        }
        async fn open(&self, name: &VolumeName) -> io::Result<Volume> {
            self.0.open(name).await
        }
        async fn delete(&self, name: &VolumeName) -> io::Result<()> {
            self.0.delete(name).await
        }
        async fn list(&self) -> io::Result<Vec<VolumeName>> {
            self.0.list().await
        }
        async fn rename(&self, from: &VolumeName, to: &VolumeName) -> io::Result<()> {
            self.0.rename(from, to).await
        }
        async fn capacity(&self) -> io::Result<Capacity> {
            let used = VOLUME_COST * self.0.list().await?.len() as u64;
            Ok(Capacity {
                data_free_bytes: POOL_TOTAL - used,
                data_total_bytes: POOL_TOTAL,
                metadata_free_bytes: POOL_TOTAL,
                metadata_total_bytes: POOL_TOTAL,
            })
        }
    }

    fn fork(image: ManifestRef) -> Source {
        Source::Fork {
            manifest: image,
            base: None,
        }
    }

    /// Publish an image whose chunk `i` holds byte `pattern[i]` (0 is a
    /// hole) and stage its volume.
    async fn staged_image(r: &Rig, pattern: &[u8]) -> ManifestRef {
        let image = ManifestRef::new();
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
        image_volumes::reconcile_once(r.disk.as_ref(), &r.store, &[image])
            .await
            .unwrap();
        image
    }

    async fn bytes_at(path: &std::path::Path) -> Vec<u8> {
        let size = std::fs::metadata(path).unwrap().len();
        let v = Volume::open(VolumeName::new("probe").unwrap(), path, size, CHUNK).unwrap();
        let mut out = Vec::new();
        for i in 0..v.chunk_count() {
            out.push(v.read_chunk(ChunkIndex(i)).await.unwrap()[0]);
        }
        out
    }

    async fn write_byte(path: &std::path::Path, chunk: u64, byte: u8) {
        let size = std::fs::metadata(path).unwrap().len();
        let v = Volume::open(VolumeName::new("probe").unwrap(), path, size, CHUNK).unwrap();
        v.write_chunk(ChunkIndex(chunk), vec![byte; CHUNK as usize])
            .await
            .unwrap();
    }

    async fn names(r: &Rig) -> Vec<String> {
        let mut names: Vec<String> = r
            .disk
            .list()
            .await
            .unwrap()
            .iter()
            .map(|n| n.as_str().to_string())
            .collect();
        names.sort();
        names
    }

    async fn checkpoints_of(r: &Rig, sandbox: SandboxId, role: DiskRole) -> Vec<ManifestRef> {
        let mut out = Vec::new();
        for name in r.disk.list().await.unwrap() {
            if let Some(Parsed {
                sandbox: s,
                role: ro,
                kind: Kind::Checkpoint(m),
            }) = parse(&name)
            {
                if s == sandbox && ro == role {
                    out.push(m);
                }
            }
        }
        out
    }

    #[test]
    fn names_say_what_a_volume_is() {
        let sandbox = SandboxId::new();
        let manifest = ManifestRef::new();
        assert_eq!(
            parse(&live_name(sandbox, DiskRole::Root)),
            Some(Parsed {
                sandbox,
                role: DiskRole::Root,
                kind: Kind::Live
            })
        );
        assert_eq!(
            parse(&checkpoint_name(sandbox, DiskRole::Swap, manifest)),
            Some(Parsed {
                sandbox,
                role: DiskRole::Swap,
                kind: Kind::Checkpoint(manifest)
            })
        );
        assert_eq!(
            parse(&transient_name(sandbox, DiskRole::Root)),
            Some(Parsed {
                sandbox,
                role: DiskRole::Root,
                kind: Kind::Transient
            })
        );
        assert_eq!(parse(&image_volumes::volume_name(manifest)), None);
        assert_eq!(parse(&VolumeName::new("sb-nothex-root").unwrap()), None);
        assert_eq!(
            parse(&VolumeName::new(format!("sb-{}-disk", sandbox.as_uuid().simple())).unwrap()),
            None
        );
        assert_eq!(
            image_volumes::image_of(&live_name(sandbox, DiskRole::Root)),
            None
        );
    }

    #[tokio::test]
    async fn provision_from_an_image_snapshots_it_and_starts_a_lineage() {
        let r = rig();
        let image = staged_image(&r, &[1, 0, 3]).await;
        let sandbox = SandboxId::new();

        let p = r
            .volumes
            .provision(
                sandbox,
                Source::Fork {
                    manifest: image,
                    base: None,
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(bytes_at(&p.root).await, vec![1, 0, 3]);
        assert_eq!(p.swap, None);
        assert_eq!(
            r.volumes.path(sandbox, DiskRole::Root).await.unwrap(),
            p.root
        );

        let lineage = checkpoints_of(&r, sandbox, DiskRole::Root).await;
        assert_eq!(lineage, vec![p.root_manifest]);
        assert_ne!(lineage[0].manifest_id, image.manifest_id);
        assert_eq!(lineage[0].version, 1);
        assert_eq!(
            r.volumes.roles(sandbox).await.unwrap(),
            vec![DiskRole::Root]
        );
        let v1 = r.store.get_manifest(lineage[0]).await.unwrap();
        assert_eq!(v1, r.store.get_manifest(image).await.unwrap());
        assert_eq!(
            names(&r).await,
            vec![
                image_volumes::volume_name(image).as_str().to_string(),
                live_name(sandbox, DiskRole::Root).as_str().to_string(),
                checkpoint_name(sandbox, DiskRole::Root, lineage[0])
                    .as_str()
                    .to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn provision_from_a_manifest_continues_its_lineage() {
        let r = rig();
        let image = staged_image(&r, &[1, 2]).await;
        let first = SandboxId::new();
        let p = r
            .volumes
            .provision(
                first,
                Source::Fork {
                    manifest: image,
                    base: None,
                },
                None,
            )
            .await
            .unwrap();
        write_byte(&p.root, 1, 9).await;
        let published = r.volumes.checkpoint(first, DiskRole::Root).await.unwrap();
        r.volumes.release(first).await.unwrap();

        let second = SandboxId::new();
        let p = r
            .volumes
            .provision(
                second,
                Source::Continue {
                    manifest: published,
                    base: Some(image),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(bytes_at(&p.root).await, vec![1, 9]);
        assert_eq!(
            checkpoints_of(&r, second, DiskRole::Root).await,
            vec![published]
        );

        // The same without a staged base.
        let third = SandboxId::new();
        let p = r
            .volumes
            .provision(
                third,
                Source::Continue {
                    manifest: published,
                    base: None,
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(bytes_at(&p.root).await, vec![1, 9]);
        assert_eq!(p.root_manifest, published);
    }

    #[tokio::test]
    async fn a_fork_of_a_published_manifest_starts_its_own_lineage() {
        let r = rig();
        let image = staged_image(&r, &[1, 2]).await;
        let first = SandboxId::new();
        let p = r
            .volumes
            .provision(
                first,
                Source::Fork {
                    manifest: image,
                    base: None,
                },
                None,
            )
            .await
            .unwrap();
        write_byte(&p.root, 1, 9).await;
        let published = r.volumes.checkpoint(first, DiskRole::Root).await.unwrap();

        let mut forks = Vec::new();
        for _ in 0..2 {
            let sandbox = SandboxId::new();
            let p = r
                .volumes
                .provision(
                    sandbox,
                    Source::Fork {
                        manifest: published,
                        base: Some(image),
                    },
                    None,
                )
                .await
                .unwrap();
            assert_eq!(bytes_at(&p.root).await, vec![1, 9]);
            assert_ne!(p.root_manifest.manifest_id, published.manifest_id);
            assert_eq!(p.root_manifest.version, 1);
            forks.push(p.root_manifest);
        }
        assert_ne!(forks[0].manifest_id, forks[1].manifest_id);
        // The shared manifest is still at the version the first sandbox published.
        assert_eq!(
            r.volumes
                .checkpoint_of(first, DiskRole::Root)
                .await
                .unwrap(),
            published
        );
    }

    #[tokio::test]
    async fn a_discarded_checkpoint_leaves_the_live_volume_and_the_last_checkpoint() {
        let r = rig();
        let image = staged_image(&r, &[1, 2]).await;
        let sandbox = SandboxId::new();
        let p = r
            .volumes
            .provision(
                sandbox,
                Source::Fork {
                    manifest: image,
                    base: None,
                },
                None,
            )
            .await
            .unwrap();
        write_byte(&p.root, 0, 7).await;
        let pending = r.volumes.take(sandbox, DiskRole::Root).await.unwrap();
        assert_eq!(pending.previous(), p.root_manifest);
        r.volumes.discard(pending).await.unwrap();
        assert_eq!(bytes_at(&p.root).await, vec![7, 2]);
        assert_eq!(
            checkpoints_of(&r, sandbox, DiskRole::Root).await,
            vec![p.root_manifest]
        );
        assert!(r.volumes.pending(sandbox, DiskRole::Root).await.is_err());
    }

    #[tokio::test]
    async fn an_empty_swap_is_zeros_with_an_empty_manifest() {
        let r = rig();
        let image = staged_image(&r, &[1]).await;
        let sandbox = SandboxId::new();
        let p = r
            .volumes
            .provision(
                sandbox,
                Source::Fork {
                    manifest: image,
                    base: None,
                },
                Some(Source::Empty {
                    size_bytes: CHUNK * 2,
                }),
            )
            .await
            .unwrap();
        let swap = p.swap.expect("swap path");
        assert_eq!(bytes_at(&swap).await, vec![0, 0]);
        let lineage = checkpoints_of(&r, sandbox, DiskRole::Swap).await;
        let manifest = r.store.get_manifest(lineage[0]).await.unwrap();
        assert!(manifest.chunks.is_empty());
        assert_eq!(manifest.total_bytes, CHUNK * 2);

        write_byte(&swap, 1, 7).await;
        let published = r.volumes.checkpoint(sandbox, DiskRole::Swap).await.unwrap();
        let manifest = r.store.get_manifest(published).await.unwrap();
        assert_eq!(
            manifest.chunks.iter().map(|c| c.offset).collect::<Vec<_>>(),
            vec![CHUNK]
        );
    }

    #[tokio::test]
    async fn a_failed_provision_leaves_nothing_of_the_sandbox() {
        let r = rig();
        let image = staged_image(&r, &[1]).await;
        let sandbox = SandboxId::new();
        let err = r
            .volumes
            .provision(
                sandbox,
                Source::Fork {
                    manifest: image,
                    base: None,
                },
                Some(Source::Continue {
                    manifest: ManifestRef::new(),
                    base: None,
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            names(&r).await,
            vec![image_volumes::volume_name(image).as_str().to_string()]
        );
    }

    #[tokio::test]
    async fn checkpoints_upload_only_what_changed_and_rotate_the_volume() {
        let r = rig();
        let image = staged_image(&r, &[1, 2, 0, 4]).await;
        let sandbox = SandboxId::new();
        let p = r
            .volumes
            .provision(
                sandbox,
                Source::Fork {
                    manifest: image,
                    base: None,
                },
                None,
            )
            .await
            .unwrap();
        let lineage = checkpoints_of(&r, sandbox, DiskRole::Root).await[0];

        write_byte(&p.root, 2, 3).await;
        let v2 = r.volumes.checkpoint(sandbox, DiskRole::Root).await.unwrap();
        assert_eq!(v2, lineage.next_version());
        let m1 = r.store.get_manifest(lineage).await.unwrap();
        let m2 = r.store.get_manifest(v2).await.unwrap();
        let delta = m2.delta_from(&m1).unwrap();
        assert_eq!(
            delta.write.iter().map(|c| c.offset).collect::<Vec<_>>(),
            vec![2 * CHUNK]
        );
        assert!(delta.zero.is_empty());
        assert_eq!(checkpoints_of(&r, sandbox, DiskRole::Root).await, vec![v2]);

        write_byte(&p.root, 0, 8).await;
        let v3 = r.volumes.checkpoint(sandbox, DiskRole::Root).await.unwrap();
        assert_eq!(v3, v2.next_version());
        let m3 = r.store.get_manifest(v3).await.unwrap();
        let delta = m3.delta_from(&m2).unwrap();
        assert_eq!(
            delta.write.iter().map(|c| c.offset).collect::<Vec<_>>(),
            vec![0]
        );
        assert_eq!(checkpoints_of(&r, sandbox, DiskRole::Root).await, vec![v3]);

        let resumed = SandboxId::new();
        let p = r
            .volumes
            .provision(
                resumed,
                Source::Continue {
                    manifest: v3,
                    base: Some(image),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(bytes_at(&p.root).await, vec![8, 2, 3, 4]);
    }

    #[tokio::test]
    async fn a_checkpoint_split_across_the_pause_publishes_the_paused_state() {
        let r = rig();
        let image = staged_image(&r, &[1]).await;
        let sandbox = SandboxId::new();
        let p = r
            .volumes
            .provision(
                sandbox,
                Source::Fork {
                    manifest: image,
                    base: None,
                },
                None,
            )
            .await
            .unwrap();
        write_byte(&p.root, 0, 5).await;
        let pending = r.volumes.take(sandbox, DiskRole::Root).await.unwrap();
        // The guest runs again before the upload.
        write_byte(&p.root, 0, 6).await;
        let published = r.volumes.publish(pending).await.unwrap();
        let m = r.store.get_manifest(published).await.unwrap();
        assert_eq!(
            m.chunks[0].hash,
            engram_chunk_store::ChunkHash::of(&vec![5; CHUNK as usize])
        );
    }

    #[tokio::test]
    async fn a_pending_checkpoint_is_found_by_name_and_says_what_changed() {
        let r = rig();
        let image = staged_image(&r, &[1, 2, 3]).await;
        let sandbox = SandboxId::new();
        let p = r
            .volumes
            .provision(
                sandbox,
                Source::Fork {
                    manifest: image,
                    base: None,
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            r.volumes
                .pending(sandbox, DiskRole::Root)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );

        write_byte(&p.root, 2, 9).await;
        let taken = r.volumes.take(sandbox, DiskRole::Root).await.unwrap();
        assert_eq!(
            r.volumes.changed(&taken).await.unwrap(),
            vec![ChunkIndex(2)]
        );
        assert_eq!(
            taken.snapshot().read_chunk(ChunkIndex(2)).await.unwrap()[0],
            9
        );
        // The process that took it is gone.
        drop(taken);
        let found = r.volumes.pending(sandbox, DiskRole::Root).await.unwrap();
        assert_eq!((found.sandbox(), found.role()), (sandbox, DiskRole::Root));
        let published = r.volumes.publish(found).await.unwrap();
        assert_eq!(
            r.store.get_manifest(published).await.unwrap().chunks.len(),
            3
        );
        assert_eq!(
            r.volumes
                .pending(sandbox, DiskRole::Root)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn a_version_the_store_holds_but_the_name_never_reached_is_skipped() {
        let r = rig();
        let image = staged_image(&r, &[1]).await;
        let sandbox = SandboxId::new();
        let p = r
            .volumes
            .provision(
                sandbox,
                Source::Fork {
                    manifest: image,
                    base: None,
                },
                None,
            )
            .await
            .unwrap();
        let lineage = checkpoints_of(&r, sandbox, DiskRole::Root).await[0];
        // A crash between `put_manifest` and the rename: v2 is in the
        // store, the snapshot is still transient.
        let m1 = r.store.get_manifest(lineage).await.unwrap();
        r.store
            .put_manifest(lineage.next_version(), &m1)
            .await
            .unwrap();
        r.disk
            .create(&transient_name(sandbox, DiskRole::Root), CHUNK)
            .await
            .unwrap();

        write_byte(&p.root, 0, 2).await;
        let published = r.volumes.checkpoint(sandbox, DiskRole::Root).await.unwrap();
        assert_eq!(published.version, 3);
        assert_eq!(
            checkpoints_of(&r, sandbox, DiskRole::Root).await,
            vec![published]
        );
        assert!(!names(&r)
            .await
            .iter()
            .any(|n| n.starts_with(TRANSIENT_PREFIX)));
    }

    #[tokio::test]
    async fn reconcile_deletes_dead_live_volumes_and_older_checkpoints() {
        let r = rig();
        let image = staged_image(&r, &[1]).await;
        let live = SandboxId::new();
        let dead = SandboxId::new();
        r.volumes
            .provision(
                live,
                Source::Fork {
                    manifest: image,
                    base: None,
                },
                Some(Source::Empty { size_bytes: CHUNK }),
            )
            .await
            .unwrap();
        r.volumes
            .provision(
                dead,
                Source::Fork {
                    manifest: image,
                    base: None,
                },
                None,
            )
            .await
            .unwrap();
        let lineage = checkpoints_of(&r, live, DiskRole::Root).await[0];
        let dead_lineage = checkpoints_of(&r, dead, DiskRole::Root).await[0];
        // A crash after the rename and before the old checkpoint's
        // delete, and a checkpoint of the live sandbox not yet published.
        let stale = r
            .disk
            .create(
                &checkpoint_name(live, DiskRole::Root, lineage.next_version()),
                CHUNK,
            )
            .await
            .unwrap();
        drop(stale);
        r.disk
            .create(&transient_name(live, DiskRole::Swap), CHUNK)
            .await
            .unwrap();
        r.disk
            .create(&transient_name(dead, DiskRole::Root), CHUNK)
            .await
            .unwrap();

        let report = r.volumes.reconcile(&[live]).await.unwrap();
        assert!(report.failed.is_empty());
        let mut deleted: Vec<String> = report
            .deleted
            .iter()
            .map(|n| n.as_str().to_string())
            .collect();
        deleted.sort();
        let mut expected = vec![
            live_name(dead, DiskRole::Root).as_str().to_string(),
            checkpoint_name(live, DiskRole::Root, lineage)
                .as_str()
                .to_string(),
            transient_name(dead, DiskRole::Root).as_str().to_string(),
        ];
        expected.sort();
        assert_eq!(deleted, expected);
        assert_eq!(
            checkpoints_of(&r, live, DiskRole::Root).await,
            vec![lineage.next_version()]
        );
        assert_eq!(
            checkpoints_of(&r, dead, DiskRole::Root).await,
            vec![dead_lineage],
            "a dead sandbox keeps its newest checkpoint for a resume here"
        );
        assert!(report.kept.contains(&dead) && report.kept.contains(&live));
        assert!(r.disk.open(&live_name(live, DiskRole::Swap)).await.is_ok());
        assert!(r
            .disk
            .open(&transient_name(live, DiskRole::Swap))
            .await
            .is_ok());
        assert!(r
            .disk
            .open(&image_volumes::volume_name(image))
            .await
            .is_ok());

        let again = r.volumes.reconcile(&[live]).await.unwrap();
        assert!(again.deleted.is_empty() && again.failed.is_empty());
    }

    #[tokio::test]
    async fn release_keeps_the_newest_checkpoint_volume_of_each_role() {
        let r = rig();
        let image = staged_image(&r, &[1]).await;
        let a = SandboxId::new();
        let b = SandboxId::new();
        for s in [a, b] {
            r.volumes
                .provision(s, fork(image), Some(Source::Empty { size_bytes: CHUNK }))
                .await
                .unwrap();
        }
        let root = r.volumes.checkpoint(a, DiskRole::Root).await.unwrap();
        let swap = checkpoints_of(&r, a, DiskRole::Swap).await[0];

        r.volumes.release(a).await.unwrap();

        let mut expected = vec![
            image_volumes::volume_name(image).as_str().to_string(),
            checkpoint_name(a, DiskRole::Root, root)
                .as_str()
                .to_string(),
            checkpoint_name(a, DiskRole::Swap, swap)
                .as_str()
                .to_string(),
        ];
        for role in DiskRole::ALL {
            expected.push(live_name(b, role).as_str().to_string());
            let lineage = checkpoints_of(&r, b, role).await[0];
            expected.push(checkpoint_name(b, role, lineage).as_str().to_string());
        }
        expected.sort();
        assert_eq!(names(&r).await, expected);
        assert_eq!(
            r.volumes.path(a, DiskRole::Root).await.unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn a_resume_on_the_same_host_snapshots_the_kept_checkpoint() {
        let r = rig();
        let image = staged_image(&r, &[1, 2]).await;
        let sandbox = SandboxId::new();
        let p = r
            .volumes
            .provision(sandbox, fork(image), None)
            .await
            .unwrap();
        write_byte(&p.root, 1, 9).await;
        let published = r.volumes.checkpoint(sandbox, DiskRole::Root).await.unwrap();
        r.volumes.release(sandbox).await.unwrap();

        let gets = r.blob.gets();
        let p = r
            .volumes
            .provision(
                sandbox,
                Source::Continue {
                    manifest: published,
                    base: Some(image),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.blob.gets(), gets, "no chunk is fetched");
        assert_eq!(bytes_at(&p.root).await, vec![1, 9]);
        assert_eq!(p.root_manifest, published);
        assert_eq!(
            checkpoints_of(&r, sandbox, DiskRole::Root).await,
            vec![published]
        );
    }

    #[tokio::test]
    async fn a_resume_of_a_newer_manifest_stages_the_delta_onto_the_kept_checkpoint() {
        let r = rig();
        let image = staged_image(&r, &[1, 2]).await;
        let sandbox = SandboxId::new();
        r.volumes
            .provision(sandbox, fork(image), None)
            .await
            .unwrap();
        let published = r.volumes.checkpoint(sandbox, DiskRole::Root).await.unwrap();
        r.volumes.release(sandbox).await.unwrap();

        // Another host published the next version with chunk 0 changed.
        let mut newer = r.store.get_manifest(published).await.unwrap();
        newer.chunks[0].hash = r.store.put_chunk(&vec![7u8; CHUNK as usize]).await.unwrap();
        let next = published.next_version();
        r.store.put_manifest(next, &newer).await.unwrap();

        let gets = r.blob.gets();
        let p = r
            .volumes
            .provision(
                sandbox,
                Source::Continue {
                    manifest: next,
                    base: Some(image),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.blob.gets(), gets + 1, "only the changed chunk is fetched");
        assert_eq!(bytes_at(&p.root).await, vec![7, 2]);
        assert_eq!(
            checkpoints_of(&r, sandbox, DiskRole::Root).await,
            vec![next]
        );
    }

    #[tokio::test]
    async fn reap_purges_kept_sandboxes_oldest_first_until_the_floor_holds() {
        let r = pressured_rig();
        let image = staged_image(&r, &[1]).await;
        let a = SandboxId::new();
        let b = SandboxId::new();
        for s in [a, b] {
            r.volumes.provision(s, fork(image), None).await.unwrap();
        }
        r.volumes.release(a).await.unwrap();
        r.volumes.release(b).await.unwrap();
        // Three volumes: the image and one kept checkpoint each.
        assert_eq!(names(&r).await.len(), 3);
        let a_checkpoint = checkpoint_name(
            a,
            DiskRole::Root,
            checkpoints_of(&r, a, DiskRole::Root).await[0],
        );

        let report = r.volumes.reap(POOL_TOTAL - 2 * VOLUME_COST).await.unwrap();
        assert!(report.failed.is_empty());
        assert_eq!(
            report.deleted,
            vec![a_checkpoint],
            "the oldest release goes first"
        );
        assert!(checkpoints_of(&r, a, DiskRole::Root).await.is_empty());
        assert_eq!(checkpoints_of(&r, b, DiskRole::Root).await.len(), 1);

        let again = r.volumes.reap(POOL_TOTAL - 2 * VOLUME_COST).await.unwrap();
        assert!(
            again.deleted.is_empty(),
            "the floor holds; nothing more goes"
        );

        let all = r.volumes.reap(POOL_TOTAL).await.unwrap();
        assert_eq!(all.deleted.len(), 1);
        assert!(checkpoints_of(&r, b, DiskRole::Root).await.is_empty());
        let none_left = r.volumes.reap(POOL_TOTAL).await.unwrap();
        assert!(none_left.deleted.is_empty() && none_left.failed.is_empty());
    }

    #[tokio::test]
    async fn reconcile_queues_dead_sandboxes_for_the_reap() {
        let r = pressured_rig();
        let image = staged_image(&r, &[1]).await;
        let dead = SandboxId::new();
        r.volumes.provision(dead, fork(image), None).await.unwrap();

        let restarted = SessionVolumes::new(r.disk.clone(), r.store.clone());
        let report = restarted.reconcile(&[]).await.unwrap();
        assert_eq!(report.kept, vec![dead]);
        assert_eq!(checkpoints_of(&r, dead, DiskRole::Root).await.len(), 1);

        let reaped = restarted.reap(POOL_TOTAL).await.unwrap();
        assert_eq!(reaped.deleted.len(), 1);
        assert!(checkpoints_of(&r, dead, DiskRole::Root).await.is_empty());
    }
}
