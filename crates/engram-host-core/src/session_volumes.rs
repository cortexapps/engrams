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

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

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
}

/// The owner of every `sb-` volume on a host.
pub struct SessionVolumes {
    disk: Arc<dyn SessionDisk>,
    store: ChunkStore,
}

impl SessionVolumes {
    pub fn new(disk: Arc<dyn SessionDisk>, store: ChunkStore) -> Self {
        Self { disk, store }
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
            let _ = self.release(sandbox).await;
        }
        provisioned
    }

    /// The checkpoint volume first, under the transient name until its
    /// manifest is in the store; then the live volume as its snapshot.
    async fn provision_role(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        source: Source,
    ) -> io::Result<(PathBuf, ManifestRef)> {
        let tmp = transient_name(sandbox, role);
        self.delete_if_present(&tmp).await?;
        let lineage = match source {
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
                lineage
            }
            Source::Continue { manifest, base } => {
                let wanted = self.store.get_manifest(manifest).await.map_err(store_err)?;
                self.stage_into(&tmp, &wanted, base).await?;
                manifest
            }
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
                lineage
            }
        };
        let checkpoint = checkpoint_name(sandbox, role, lineage);
        self.disk.rename(&tmp, &checkpoint).await?;
        let checkpoint = self.disk.open(&checkpoint).await?;
        let live = self
            .disk
            .snapshot(&checkpoint, &live_name(sandbox, role))
            .await?;
        Ok((live.path, lineage))
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

    /// Delete every volume of the sandbox. Fails while a process holds
    /// one open.
    pub async fn release(&self, sandbox: SandboxId) -> io::Result<()> {
        let mut first_error = None;
        for name in self.disk.list().await? {
            if parse(&name).is_some_and(|p| p.sandbox == sandbox) {
                if let Err(e) = self.disk.delete(&name).await {
                    first_error.get_or_insert(e);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Delete what no live sandbox needs: every volume of a sandbox not
    /// in `live` and every checkpoint volume but the newest of its role.
    /// A live sandbox keeps its transient volume: it may be a checkpoint
    /// whose publish is being redriven. Run once after the live set is
    /// known, before any sandbox is provisioned.
    pub async fn reconcile(&self, live: &[SandboxId]) -> io::Result<Report> {
        let mut report = Report::default();
        let mut newest: std::collections::HashMap<(SandboxId, DiskRole), u64> = Default::default();
        let parsed: Vec<(VolumeName, Parsed)> = self
            .disk
            .list()
            .await?
            .into_iter()
            .filter_map(|name| parse(&name).map(|p| (name, p)))
            .collect();
        for (_, p) in &parsed {
            if let Kind::Checkpoint(m) = p.kind {
                let v = newest.entry((p.sandbox, p.role)).or_insert(0);
                *v = (*v).max(m.version);
            }
        }
        for (name, p) in parsed {
            let unwanted = !live.contains(&p.sandbox)
                || match p.kind {
                    Kind::Live | Kind::Transient => false,
                    Kind::Checkpoint(m) => m.version < newest[&(p.sandbox, p.role)],
                };
            if !unwanted {
                continue;
            }
            match self.disk.delete(&name).await {
                Ok(()) => report.deleted.push(name),
                Err(e) => report.failed.push((name, e)),
            }
        }
        Ok(report)
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

    const CHUNK: u64 = 4096;

    struct Rig {
        _dir: tempfile::TempDir,
        disk: Arc<FileDisk>,
        store: ChunkStore,
        volumes: SessionVolumes,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let disk = Arc::new(FileDisk::new(dir.path().join("volumes"), CHUNK).unwrap());
        let store = ChunkStore::new(Arc::new(LocalBlobStorage::new(dir.path().join("blobs"))));
        Rig {
            volumes: SessionVolumes::new(disk.clone(), store.clone()),
            disk,
            store,
            _dir: dir,
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
    async fn reconcile_keeps_live_sandboxes_and_the_newest_checkpoint_only() {
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
            checkpoint_name(dead, DiskRole::Root, dead_lineage)
                .as_str()
                .to_string(),
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
    async fn release_deletes_one_sandbox_and_leaves_the_rest() {
        let r = rig();
        let image = staged_image(&r, &[1]).await;
        let a = SandboxId::new();
        let b = SandboxId::new();
        for s in [a, b] {
            r.volumes
                .provision(
                    s,
                    Source::Fork {
                        manifest: image,
                        base: None,
                    },
                    Some(Source::Empty { size_bytes: CHUNK }),
                )
                .await
                .unwrap();
        }
        r.volumes.release(a).await.unwrap();
        let left = names(&r).await;
        assert_eq!(left.len(), 5);
        assert!(left
            .iter()
            .all(|n| !n.contains(&a.as_uuid().simple().to_string())));
        assert_eq!(
            r.volumes.path(a, DiskRole::Root).await.unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
