//! A session's disks as volumes. Per role (root, swap) a sandbox has a
//! live volume the hypervisor opens and one checkpoint volume. The
//! names are the only record:
//!
//! ```text
//! sb-<sandbox>-<role>                        live
//! sb-<sandbox>-<role>-ckpt-<manifest>-v<N>   the last checkpoint; holds that manifest
//! tmp-sb-<sandbox>-<role>                    a stage or a checkpoint in flight
//! sb-<sandbox>-<role>-dest-<manifest>-v<N>   a hydrating live volume's destination; becomes that manifest
//! sb-<sandbox>-<role>-base-<manifest>-v<N>   what the destination was snapshotted from; holds that manifest
//! ```
//!
//! A checkpoint volume is named after a manifest the chunk store holds,
//! so the name is written last: a crash leaves a `tmp-` volume, never a
//! name that lies. The manifest id in the name is the session's lineage
//! and the next checkpoint publishes under it, so no caller tracks a
//! manifest ref between calls.
//!
//! A role whose manifest is not on the host starts on a hydrating
//! device: the live volume reads the base's blocks and a source the
//! fill engine serves, and the kernel copies the difference in behind
//! the guest ([`SessionDisk::hydrate`]). Until that device settles there
//! is no checkpoint volume; the `dest` and `base` names record what
//! the live data converges to and what it is measured against.
//!
//! A released sandbox keeps its newest checkpoint volume per role, so a
//! resume on this host is a snapshot of it instead of a stage from blob
//! storage. [`SessionVolumes::reap`] deletes kept checkpoints, oldest
//! release first, when the pool needs the space.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use engram_chunk_store::{ChunkSize, Manifest, ManifestKind};
use engram_core::types::ids::SandboxId;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::snapshot::DiskRole;
use engram_fill::{Class, FillEngine, PeerSource};
use tracing::Instrument as _;
use uuid::Uuid;

use crate::disk::{
    Capacity, Hydration, SessionDisk, SourceDevice, SourceServer, Volume, VolumeName,
};
use crate::disk_flows::{checkpoint_split, hydration_plan, prefix_hashes, shape_matches, stage};
use crate::image_volumes::{self, ImageVolumes};

const LIVE_PREFIX: &str = "sb-";
const TRANSIENT_PREFIX: &str = "tmp-sb-";
const CHECKPOINT_INFIX: &str = "-ckpt-";
const DEST_INFIX: &str = "-dest-";
const BASE_INFIX: &str = "-base-";
/// The base of a hydration that started from zeros.
const BASE_NONE: &str = "none";
const PRECOPY_SUFFIX: &str = "-pre";

/// What a volume name says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Live,
    Checkpoint(ManifestRef),
    Transient,
    /// The destination of a hydrating live volume; holds the manifest
    /// once the hydration settles.
    Dest(ManifestRef),
    /// What a hydration's destination was snapshotted from.
    Base(Option<ManifestRef>),
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
    named_after(sandbox, role, CHECKPOINT_INFIX, &manifest_suffix(manifest))
}

fn dest_name(sandbox: SandboxId, role: DiskRole, manifest: ManifestRef) -> VolumeName {
    named_after(sandbox, role, DEST_INFIX, &manifest_suffix(manifest))
}

fn base_name(sandbox: SandboxId, role: DiskRole, manifest: Option<ManifestRef>) -> VolumeName {
    let suffix = manifest.map_or(BASE_NONE.to_string(), manifest_suffix);
    named_after(sandbox, role, BASE_INFIX, &suffix)
}

fn named_after(sandbox: SandboxId, role: DiskRole, infix: &str, suffix: &str) -> VolumeName {
    VolumeName::new(format!(
        "{LIVE_PREFIX}{}-{role}{infix}{suffix}",
        sandbox.as_uuid().simple(),
    ))
    .expect("a named volume is at most 100 bytes of [a-z0-9-]")
}

fn manifest_suffix(manifest: ManifestRef) -> String {
    format!("{}-v{}", manifest.manifest_id.simple(), manifest.version)
}

fn parse_manifest_suffix(suffix: &str) -> Option<ManifestRef> {
    let (id, version) = suffix.split_once("-v")?;
    Some(ManifestRef {
        manifest_id: Uuid::try_parse(id).ok()?,
        version: version.parse().ok()?,
    })
}
/// How [`SessionVolumes`] brought up a role's disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StagePath {
    /// The role's checkpoint was already on this host.
    Kept,
    /// A hydrating device over a snapshot of the nearest base this host
    /// holds: the guest starts at once and the kernel copies the
    /// difference in behind it.
    Lazy,
    /// The difference from the nearest base written in full before the
    /// guest starts: this platform has no hydrating device.
    Staged,
    /// A snapshot of a manifest this host had staged.
    StagedSnapshot,
    /// A fork of the image's volume.
    ImageFork,
    /// A new empty volume.
    Empty,
}

impl StagePath {
    pub fn as_str(self) -> &'static str {
        match self {
            StagePath::Kept => "kept",
            StagePath::Lazy => "lazy",
            StagePath::Staged => "staged",
            StagePath::StagedSnapshot => "staged_snapshot",
            StagePath::ImageFork => "image_fork",
            StagePath::Empty => "empty",
        }
    }
}

fn transient_name(sandbox: SandboxId, role: DiskRole) -> VolumeName {
    VolumeName::new(format!(
        "{TRANSIENT_PREFIX}{}-{}",
        sandbox.as_uuid().simple(),
        role
    ))
    .expect("a transient volume name is 44 bytes of [a-z0-9-]")
}

/// The snapshot a live move takes while the guest runs; it lives
/// beside the transient one the capture takes under the pause.
fn precopy_name(sandbox: SandboxId, role: DiskRole) -> VolumeName {
    VolumeName::new(format!(
        "{TRANSIENT_PREFIX}{}-{}{PRECOPY_SUFFIX}",
        sandbox.as_uuid().simple(),
        role
    ))
    .expect("a precopy volume name is 48 bytes of [a-z0-9-]")
}

/// What `name` says; `None` for a volume that is not a session's.
pub fn parse(name: &VolumeName) -> Option<Parsed> {
    let (rest, transient) = match name.as_str().strip_prefix(TRANSIENT_PREFIX) {
        Some(rest) => (rest, true),
        None => (name.as_str().strip_prefix(LIVE_PREFIX)?, false),
    };
    let (sandbox, rest) = rest.split_once('-')?;
    let sandbox = SandboxId::from(Uuid::try_parse(sandbox).ok()?);
    let infixed = [CHECKPOINT_INFIX, DEST_INFIX, BASE_INFIX]
        .into_iter()
        .find_map(|infix| {
            rest.split_once(infix)
                .map(|(role, suffix)| (role, infix, suffix))
        });
    let (role, kind) = match (transient, infixed) {
        (true, None) => (
            rest.strip_suffix(PRECOPY_SUFFIX).unwrap_or(rest),
            Kind::Transient,
        ),
        (false, None) => (rest, Kind::Live),
        (false, Some((role, CHECKPOINT_INFIX, suffix))) => {
            (role, Kind::Checkpoint(parse_manifest_suffix(suffix)?))
        }
        (false, Some((role, DEST_INFIX, suffix))) => {
            (role, Kind::Dest(parse_manifest_suffix(suffix)?))
        }
        (false, Some((role, BASE_INFIX, BASE_NONE))) => (role, Kind::Base(None)),
        (false, Some((role, BASE_INFIX, suffix))) => {
            (role, Kind::Base(Some(parse_manifest_suffix(suffix)?)))
        }
        _ => return None,
    };
    Some(Parsed {
        sandbox,
        role: role.parse().ok()?,
        kind,
    })
}

/// Live volumes first: a hydrating one holds its destination open, so
/// the destination can go only after it.
fn live_first(names: &mut [(VolumeName, Parsed)]) {
    names.sort_by_key(|(_, p)| p.kind != Kind::Live);
}

/// Where a live volume's contents come from. A manifest that is itself a
/// staged image is snapshotted. Otherwise, when `base` names an image
/// staged on this host, the volume starts as a snapshot of it and only
/// the difference comes in: behind the running guest where the platform
/// has a hydrating device, in full before it starts elsewhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// An image's disk: the volume starts a new lineage as a snapshot of
    /// the image's volume, which is staged first when the host has none
    /// (see [`ImageVolumes::fork`]).
    Image { manifest: ManifestRef },
    /// A manifest many sandboxes share: the volume starts a new lineage
    /// that holds a copy of it.
    Fork {
        manifest: ManifestRef,
        base: Option<ManifestRef>,
    },
    /// The sandbox's own manifest: the volume continues its lineage.
    /// `first_touch` lists the chunks the guest read before the copy
    /// reached them on its last hydrating restore, in order; the ones
    /// this restore must copy are fetched first.
    Continue {
        manifest: ManifestRef,
        base: Option<ManifestRef>,
        first_touch: Vec<u32>,
    },
    /// Zeros, in a new lineage.
    Empty { size_bytes: u64 },
}

impl Source {
    fn kind(&self) -> &'static str {
        match self {
            Source::Image { .. } => "image",
            Source::Fork { .. } => "fork",
            Source::Continue { .. } => "continue",
            Source::Empty { .. } => "empty",
        }
    }
}

/// What `provision` made: the device paths a backend opens and the
/// lineage each volume checkpoints under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provisioned {
    pub root: PathBuf,
    pub root_manifest: ManifestRef,
    /// How the root disk came up.
    pub root_stage: StagePath,
    pub swap: Option<PathBuf>,
    pub swap_manifest: Option<ManifestRef>,
    pub swap_stage: Option<StagePath>,
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

/// A volume on this host that holds a manifest near the one wanted:
/// what a role's volume starts from.
struct Base {
    volume: Volume,
    manifest: Manifest,
    manifest_ref: ManifestRef,
    /// The sandbox whose kept checkpoint volume this is; a hydration
    /// takes the volume over as its base.
    kept_by: Option<SandboxId>,
}

/// What a live volume this host does not hold is brought up to: the
/// manifest, the lineage it continues, the base to start from, and the
/// chunks the guest read first on the last hydrating restore.
struct Wanted<'a> {
    manifest: &'a Manifest,
    lineage: ManifestRef,
    base: Option<Base>,
    first_touch: &'a [u32],
}

/// The owner of every `sb-` volume on a host.
pub struct SessionVolumes {
    disk: Arc<dyn SessionDisk>,
    fill: FillEngine,
    images: Arc<ImageVolumes>,
    /// Released sandboxes whose checkpoint volumes stay, oldest release
    /// first. A sandbox `reconcile` finds with no live volume joins at
    /// the front: its age is unknown, so it goes first.
    kept: Mutex<VecDeque<SandboxId>>,
    /// Serves the sources hydrating volumes copy from; without it every
    /// volume is written in full before its guest starts.
    sources: Option<Arc<dyn SourceServer>>,
    /// The source of each hydrating live volume this process serves.
    serving: Mutex<HashMap<VolumeName, Box<dyn SourceDevice>>>,
    /// What each stopped source recorded as read first, kept until the
    /// live volume goes.
    touched: Mutex<HashMap<VolumeName, Vec<u32>>>,
}

impl SessionVolumes {
    pub fn new(disk: Arc<dyn SessionDisk>, fill: FillEngine) -> Self {
        Self {
            images: Arc::new(ImageVolumes::new(disk.clone(), fill.clone())),
            disk,
            fill,
            kept: Mutex::new(VecDeque::new()),
            sources: None,
            serving: Mutex::new(HashMap::new()),
            touched: Mutex::new(HashMap::new()),
        }
    }

    /// Start guests on hydrating volumes, with `sources` serving what
    /// the kernel copies from.
    pub fn with_sources(mut self, sources: Arc<dyn SourceServer>) -> Self {
        self.sources = Some(sources);
        self
    }

    /// The source server and the copy unit, when this host hydrates.
    fn hydration_support(&self) -> Option<(&dyn SourceServer, u64)> {
        Some((self.sources.as_deref()?, self.disk.region_bytes()?))
    }

    pub fn disk(&self) -> &dyn SessionDisk {
        self.disk.as_ref()
    }

    /// The image volumes of the same disk.
    pub fn images(&self) -> &Arc<ImageVolumes> {
        &self.images
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
        self.provision_with_peer(sandbox, root, swap, None).await
    }

    /// [`Self::provision`], with each chunk a stage fetches asked of
    /// `peer` (a host that holds the session's chunks) before the store.
    pub async fn provision_with_peer(
        &self,
        sandbox: SandboxId,
        root: Source,
        swap: Option<Source>,
        peer: Option<Arc<dyn PeerSource>>,
    ) -> io::Result<Provisioned> {
        let fill = match peer {
            Some(peer) => self.fill.with_peer(peer),
            None => self.fill.clone(),
        };
        // A sandbox resumed on this host is live again: its kept
        // checkpoint is no longer the reap's to purge.
        self.kept.lock().unwrap().retain(|s| *s != sandbox);
        // The roles use separate volumes, so they provision together. Both
        // run to the end before a purge: a role dropped mid-way can still
        // make a thin volume that has no name, which the purge cannot find.
        let swap = async {
            match swap {
                Some(source) => self
                    .provision_role(&fill, sandbox, DiskRole::Swap, source)
                    .await
                    .map(|(path, manifest, stage)| (Some(path), Some(manifest), Some(stage))),
                None => Ok((None, None, None)),
            }
        };
        match tokio::join!(
            self.provision_role(&fill, sandbox, DiskRole::Root, root),
            swap
        ) {
            (Ok((root, root_manifest, root_stage)), Ok((swap, swap_manifest, swap_stage))) => {
                Ok(Provisioned {
                    root,
                    root_manifest,
                    root_stage,
                    swap,
                    swap_manifest,
                    swap_stage,
                })
            }
            (Err(error), _) | (_, Err(error)) => {
                let _ = self.purge(sandbox).await;
                Err(error)
            }
        }
    }

    /// The live volume of a role. A manifest this host holds in full (a
    /// staged image, a kept checkpoint of it) is snapshotted; a
    /// `Continue` or `Fork` of one it does not hold starts from the
    /// nearest base it has ([`Self::bring_up`]).
    #[tracing::instrument(
        name = "disk.provision",
        skip_all,
        fields(sandbox_id = %sandbox, %role, source = source.kind())
    )]
    async fn provision_role(
        &self,
        fill: &FillEngine,
        sandbox: SandboxId,
        role: DiskRole,
        source: Source,
    ) -> io::Result<(PathBuf, ManifestRef, StagePath)> {
        let tmp = transient_name(sandbox, role);
        self.delete_if_present(&tmp).await?;
        let (live, lineage, path) = match source {
            Source::Image { manifest } => {
                let wanted = self.manifest(manifest).await?;
                self.images
                    .fork(manifest, &tmp)
                    .instrument(tracing::info_span!("disk.image_fork"))
                    .await?;
                let lineage = self.new_lineage(&wanted).await?;
                let live = self.commit_live(&tmp, sandbox, role, lineage).await?;
                (live, lineage, StagePath::ImageFork)
            }
            Source::Fork { manifest, base } => {
                let wanted = self.manifest(manifest).await?;
                let lineage = self.new_lineage(&wanted).await?;
                match self.staged(manifest).await {
                    Some((origin, _)) => {
                        self.disk.snapshot(&origin, &tmp).await?;
                        let live = self.commit_live(&tmp, sandbox, role, lineage).await?;
                        (live, lineage, StagePath::StagedSnapshot)
                    }
                    None => {
                        let base = self.image_base(base).await;
                        let (live, path) = self
                            .bring_up(
                                fill,
                                sandbox,
                                role,
                                Wanted {
                                    manifest: &wanted,
                                    lineage,
                                    base,
                                    first_touch: &[],
                                },
                            )
                            .await?;
                        (live, lineage, path)
                    }
                }
            }
            Source::Continue {
                manifest,
                base,
                first_touch,
            } => match self.kept(sandbox, role, manifest).await? {
                Some((owner, volume, _, held)) if held == manifest => {
                    let checkpoint = self.adopt(owner, volume, sandbox, role, held).await?;
                    let live = self.live_snapshot(&checkpoint, sandbox, role).await?;
                    (live, manifest, StagePath::Kept)
                }
                kept => {
                    let wanted = self.manifest(manifest).await?;
                    let base = match kept {
                        Some((owner, volume, held_manifest, held))
                            if volume.size_bytes == wanted.total_bytes =>
                        {
                            Some(Base {
                                volume,
                                manifest: held_manifest,
                                manifest_ref: held,
                                kept_by: Some(owner),
                            })
                        }
                        _ => self.image_base(base).await,
                    };
                    let (live, path) = self
                        .bring_up(
                            fill,
                            sandbox,
                            role,
                            Wanted {
                                manifest: &wanted,
                                lineage: manifest,
                                base,
                                first_touch: &first_touch,
                            },
                        )
                        .await?;
                    (live, manifest, path)
                }
            },
            Source::Empty { size_bytes } => {
                let volume = self.disk.create(&tmp, size_bytes).await?;
                let manifest = Manifest {
                    chunk_size: ChunkSize::bytes(volume.chunk_size),
                    ..Manifest::empty(ManifestKind::Disk, size_bytes)
                };
                let lineage = self.new_lineage(&manifest).await?;
                let live = self.commit_live(&tmp, sandbox, role, lineage).await?;
                (live, lineage, StagePath::Empty)
            }
        };
        Ok((live.path, lineage, path))
    }

    async fn manifest(&self, manifest: ManifestRef) -> io::Result<Manifest> {
        self.fill
            .store()
            .get_manifest(manifest)
            .instrument(tracing::info_span!("disk.manifest_get"))
            .await
            .map_err(store_err)
    }

    /// Start a lineage that holds `manifest`.
    async fn new_lineage(&self, manifest: &Manifest) -> io::Result<ManifestRef> {
        let lineage = ManifestRef::new();
        self.fill
            .store()
            .put_manifest(lineage, manifest)
            .instrument(tracing::info_span!("disk.manifest_put"))
            .await
            .map_err(store_err)?;
        Ok(lineage)
    }

    /// Commit `tmp`, which holds `lineage`'s manifest, as the role's
    /// checkpoint volume, and make the live volume its snapshot.
    async fn commit_live(
        &self,
        tmp: &VolumeName,
        sandbox: SandboxId,
        role: DiskRole,
        lineage: ManifestRef,
    ) -> io::Result<Volume> {
        let checkpoint = self.commit(tmp, sandbox, role, lineage).await?;
        self.live_snapshot(&checkpoint, sandbox, role).await
    }

    async fn live_snapshot(
        &self,
        checkpoint: &Volume,
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<Volume> {
        self.disk
            .snapshot(checkpoint, &live_name(sandbox, role))
            .instrument(tracing::info_span!("disk.live_snapshot"))
            .await
    }

    /// Give the staged transient volume its checkpoint name and delete
    /// the role's other checkpoints, and any checkpoint of the same
    /// lineage a released sandbox kept on this host.
    async fn commit(
        &self,
        tmp: &VolumeName,
        sandbox: SandboxId,
        role: DiskRole,
        lineage: ManifestRef,
    ) -> io::Result<Volume> {
        let name = checkpoint_name(sandbox, role, lineage);
        self.disk.rename(tmp, &name).await?;
        let released = self.released();
        for other in self.disk.list().await? {
            if other != name
                && parse(&other).is_some_and(|p| {
                    p.role == role
                        && match p.kind {
                            Kind::Checkpoint(m) => {
                                p.sandbox == sandbox
                                    || (m.manifest_id == lineage.manifest_id
                                        && released.contains(&p.sandbox))
                            }
                            _ => false,
                        }
                })
            {
                self.disk.delete(&other).await?;
            }
        }
        self.disk.open(&name).await
    }

    /// The live volume of a role whose manifest is not on this host: a
    /// hydrating device when this host has one and serves sources, else
    /// the difference from the base written in full first.
    async fn bring_up(
        &self,
        fill: &FillEngine,
        sandbox: SandboxId,
        role: DiskRole,
        wanted: Wanted<'_>,
    ) -> io::Result<(Volume, StagePath)> {
        if self.hydration_support().is_some() {
            let live = self
                .hydrate(fill, sandbox, role, wanted)
                .instrument(tracing::info_span!(
                    "disk.hydrate",
                    held = tracing::field::Empty,
                    delta = tracing::field::Empty,
                    zero = tracing::field::Empty,
                    prefix = tracing::field::Empty,
                ))
                .await?;
            return Ok((live, StagePath::Lazy));
        }
        let tmp = transient_name(sandbox, role);
        stage(
            self.disk.as_ref(),
            fill,
            Class::Demand,
            &tmp,
            wanted.manifest,
            wanted.base.as_ref().map(|b| (&b.volume, &b.manifest)),
        )
        .instrument(tracing::info_span!("disk.stage"))
        .await?;
        let live = self
            .commit_live(&tmp, sandbox, role, wanted.lineage)
            .await?;
        Ok((live, StagePath::Staged))
    }

    /// A hydrating live volume: its destination is a snapshot of the
    /// base (a kept checkpoint volume is taken over; an empty volume
    /// stands in for no base) under the `dest` name, and its source
    /// serves `wanted` through `fill`. Chunks the base holds in final
    /// form are skipped; chunks `wanted` leaves out are zeroed first.
    /// The `first_touch` chunks the copy must bring in are fetched at
    /// `Prefix` without waiting, so the guest's first reads find them
    /// resident before the copy reaches them.
    /// Records on the current span how many chunks the base `held`, the
    /// kernel copies (`delta`), were zeroed (`zero`) and make the `prefix`.
    async fn hydrate(
        &self,
        fill: &FillEngine,
        sandbox: SandboxId,
        role: DiskRole,
        wanted: Wanted<'_>,
    ) -> io::Result<Volume> {
        let Wanted {
            manifest: wanted,
            lineage,
            base,
            first_touch,
        } = wanted;
        let (sources, region_bytes) = self.hydration_support().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "this host does not hydrate")
        })?;
        if let Some(base) = &base {
            shape_matches(&base.volume, &base.manifest)?;
        }
        let plan = hydration_plan(wanted, base.as_ref().map(|b| &b.manifest))?;
        let base_volume = match base {
            Some(Base {
                volume,
                manifest_ref,
                kept_by: Some(_),
                ..
            }) => {
                let name = base_name(sandbox, role, Some(manifest_ref));
                self.disk.rename(&volume.name, &name).await?;
                drop(volume);
                self.disk.open(&name).await?
            }
            Some(Base {
                volume,
                manifest_ref,
                kept_by: None,
                ..
            }) => {
                self.disk
                    .snapshot(&volume, &base_name(sandbox, role, Some(manifest_ref)))
                    .await?
            }
            None => {
                self.disk
                    .create(&base_name(sandbox, role, None), wanted.total_bytes)
                    .await?
            }
        };
        let dest = self
            .disk
            .snapshot(&base_volume, &dest_name(sandbox, role, lineage))
            .await?;
        drop(base_volume);
        shape_matches(&dest, wanted)?;
        for chunk in &plan.zero {
            dest.zero_chunk(*chunk).await?;
        }
        let source = sources
            .serve(fill.clone(), wanted, plan.delta.clone(), region_bytes)
            .await?;
        let live = match self
            .disk
            .hydrate(&dest, source.path(), &live_name(sandbox, role), &plan.skip)
            .await
        {
            Ok(live) => live,
            Err(e) => {
                if let Err(error) = source.stop().await {
                    tracing::warn!(sandbox_id = %sandbox, %role, %error, "source of a failed hydration not stopped");
                }
                return Err(e);
            }
        };
        let prefix = prefix_hashes(first_touch, &plan.delta, wanted);
        let span = tracing::Span::current();
        span.record("held", (plan.skip.len() - plan.zero.len()) as u64);
        span.record("delta", plan.delta.len() as u64);
        span.record("zero", plan.zero.len() as u64);
        span.record("prefix", prefix.len() as u64);
        if !prefix.is_empty() {
            let fill = fill.clone();
            tokio::spawn(
                async move {
                    match fill.fill(prefix, Class::Prefix).await {
                        Ok(stats) => tracing::info!(?stats, "disk first-touch prefix resident"),
                        Err(error) => tracing::warn!(%error, "disk first-touch prefix failed"),
                    }
                }
                .instrument(tracing::info_span!("disk.prefix", sandbox_id = %sandbox, %role)),
            );
        }
        self.serving
            .lock()
            .unwrap()
            .insert(live.name.clone(), source);
        Ok(live)
    }

    /// The staged volume of `image` as a base, when this host has it.
    async fn image_base(&self, image: Option<ManifestRef>) -> Option<Base> {
        let image = image?;
        let (volume, manifest) = self.staged(image).await?;
        Some(Base {
            volume,
            manifest,
            manifest_ref: image,
            kept_by: None,
        })
    }

    /// The newest checkpoint volume of `role` that `sandbox` or a released
    /// sandbox kept for the lineage of `wanted`, no newer than it: the
    /// sandbox that kept it, the volume, its manifest and ref. A resume
    /// runs in a new sandbox, so the volume is found by lineage. A live
    /// sandbox's checkpoint is never a candidate: a same-host move's source
    /// shares the lineage with its destination while both run.
    async fn kept(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        wanted: ManifestRef,
    ) -> io::Result<Option<(SandboxId, Volume, Manifest, ManifestRef)>> {
        let released = self.released();
        let newest = self
            .disk
            .list()
            .await?
            .iter()
            .filter_map(parse)
            .filter_map(|p| match p.kind {
                Kind::Checkpoint(m)
                    if p.role == role
                        && (p.sandbox == sandbox || released.contains(&p.sandbox))
                        && m.manifest_id == wanted.manifest_id
                        && m.version <= wanted.version =>
                {
                    Some((p.sandbox, m))
                }
                _ => None,
            })
            .max_by_key(|(_, m)| m.version);
        let Some((owner, held)) = newest else {
            return Ok(None);
        };
        let volume = self.disk.open(&checkpoint_name(owner, role, held)).await?;
        Ok(match self.fill.store().get_manifest(held).await {
            Ok(manifest) => Some((owner, volume, manifest, held)),
            Err(_) => None,
        })
    }

    /// Make the checkpoint `owner` kept for `held` the checkpoint of
    /// `sandbox`: a rename, so no data moves and open handles stay valid.
    async fn adopt(
        &self,
        owner: SandboxId,
        volume: Volume,
        sandbox: SandboxId,
        role: DiskRole,
        held: ManifestRef,
    ) -> io::Result<Volume> {
        if owner == sandbox {
            return Ok(volume);
        }
        let name = checkpoint_name(sandbox, role, held);
        self.disk
            .rename(&checkpoint_name(owner, role, held), &name)
            .await?;
        self.disk.open(&name).await
    }

    /// The staged volume of an image and its manifest, when this host
    /// has both.
    async fn staged(&self, image: ManifestRef) -> Option<(Volume, Manifest)> {
        match (
            self.disk.open(&image_volumes::volume_name(image)).await,
            self.fill.store().get_manifest(image).await,
        ) {
            (Ok(volume), Ok(manifest)) => Some((volume, manifest)),
            _ => None,
        }
    }

    /// One step of a hydrating role's way to a settled volume: its
    /// progress, and once every region is in, the cutover onto the
    /// destination and the end of its source. `Settled` when the live
    /// volume maps straight onto its destination.
    pub async fn settle(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<Hydration> {
        let live = live_name(sandbox, role);
        match self.disk.hydration(&live).await? {
            state @ Hydration::Hydrating {
                hydrated, total, ..
            } if hydrated < total => return Ok(state),
            Hydration::Hydrating { .. } => {
                let (dest, _) = self.lazy_layout(sandbox, role).await?;
                let dest = self.disk.open(&dest).await?;
                self.disk.settle(&live, &dest).await?;
            }
            Hydration::Settled => {}
        }
        self.stop_source(&live).await?;
        Ok(Hydration::Settled)
    }

    /// The roles of `sandbox` whose live volume is still hydrating.
    pub async fn hydrating(&self, sandbox: SandboxId) -> io::Result<Vec<DiskRole>> {
        let mut roles = Vec::new();
        for role in self.lazy_roles(sandbox).await? {
            if let Hydration::Hydrating { .. } =
                self.disk.hydration(&live_name(sandbox, role)).await?
            {
                roles.push(role);
            }
        }
        Ok(roles)
    }

    /// After a restart: serve again the sources of `sandbox`'s hydrating
    /// roles, which the process that died left to the kernel. Returns
    /// those roles.
    pub async fn reattach(&self, sandbox: SandboxId) -> io::Result<Vec<DiskRole>> {
        let Some((sources, region_bytes)) = self.hydration_support() else {
            return Ok(Vec::new());
        };
        let mut roles = Vec::new();
        for role in self.lazy_roles(sandbox).await? {
            let live = live_name(sandbox, role);
            if self.serving.lock().unwrap().contains_key(&live) {
                continue;
            }
            let Hydration::Hydrating { source, .. } = self.disk.hydration(&live).await? else {
                continue;
            };
            let (dest, base) = self.lazy_layout(sandbox, role).await?;
            let (Some(Kind::Dest(wanted)), Some(Kind::Base(base))) =
                (parse(&dest).map(|p| p.kind), parse(&base).map(|p| p.kind))
            else {
                unreachable!("lazy_layout returns a dest and a base name");
            };
            let wanted = self.manifest(wanted).await?;
            let base = match base {
                Some(base) => Some(self.manifest(base).await?),
                None => None,
            };
            let plan = hydration_plan(&wanted, base.as_ref())?;
            let recovered = sources
                .recover(
                    &source,
                    self.fill.clone(),
                    &wanted,
                    plan.delta,
                    region_bytes,
                )
                .await?;
            self.serving.lock().unwrap().insert(live, recovered);
            roles.push(role);
        }
        Ok(roles)
    }

    /// [`Self::reattach`] for every sandbox with a hydrating role, live
    /// or not: a volume whose source nothing serves cannot be deleted,
    /// so this runs before the startup reconcile. A role this process
    /// already serves is skipped. A sandbox whose reattach fails is
    /// logged and left as it is (its volumes stay, unserved and
    /// undeletable) so the rest of the host comes up.
    pub async fn reattach_all(&self) -> io::Result<Vec<(SandboxId, DiskRole)>> {
        let mut sandboxes: Vec<SandboxId> = Vec::new();
        for p in self.disk.list().await?.iter().filter_map(parse) {
            if matches!(p.kind, Kind::Dest(_)) && !sandboxes.contains(&p.sandbox) {
                sandboxes.push(p.sandbox);
            }
        }
        let mut reattached = Vec::new();
        for sandbox in sandboxes {
            match self.reattach(sandbox).await {
                Ok(roles) => reattached.extend(roles.into_iter().map(|role| (sandbox, role))),
                Err(error) => {
                    tracing::error!(sandbox_id = %sandbox, %error, "hydrating volume not served again");
                }
            }
        }
        Ok(reattached)
    }

    /// The chunks the guest read before the copy reached them, in
    /// order: from the role's source while it serves, and after its stop
    /// until the volume goes. Empty for a role that never hydrated.
    pub fn first_touch(&self, sandbox: SandboxId, role: DiskRole) -> Vec<u32> {
        let live = live_name(sandbox, role);
        if let Some(source) = self.serving.lock().unwrap().get(&live) {
            return source.touched();
        }
        self.touched
            .lock()
            .unwrap()
            .get(&live)
            .cloned()
            .unwrap_or_default()
    }

    /// The devices of the sources this process serves.
    pub fn sources(&self) -> Vec<PathBuf> {
        self.serving
            .lock()
            .unwrap()
            .values()
            .map(|s| s.path().to_path_buf())
            .collect()
    }

    /// The roles of `sandbox` brought up on a hydrating device whose
    /// first checkpoint has not run: they have a `dest` volume.
    async fn lazy_roles(&self, sandbox: SandboxId) -> io::Result<Vec<DiskRole>> {
        Ok(self
            .disk
            .list()
            .await?
            .iter()
            .filter_map(parse)
            .filter(|p| p.sandbox == sandbox && matches!(p.kind, Kind::Dest(_)))
            .map(|p| p.role)
            .collect())
    }

    /// The `dest` and `base` volume names of a lazy role.
    async fn lazy_layout(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<(VolumeName, VolumeName)> {
        let (mut dest, mut base) = (None, None);
        for name in self.disk.list().await? {
            match parse(&name) {
                Some(p) if p.sandbox == sandbox && p.role == role => match p.kind {
                    Kind::Dest(_) => dest = Some(name),
                    Kind::Base(_) => base = Some(name),
                    _ => {}
                },
                _ => {}
            }
        }
        match (dest, base) {
            (Some(dest), Some(base)) => Ok((dest, base)),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("sandbox {sandbox} has no hydrating {role} volume"),
            )),
        }
    }

    /// End the source of `live`, when this process serves one. The
    /// device that read it must be gone or settled first.
    async fn stop_source(&self, live: &VolumeName) -> io::Result<()> {
        let source = self.serving.lock().unwrap().remove(live);
        match source {
            Some(source) => {
                self.touched
                    .lock()
                    .unwrap()
                    .insert(live.clone(), source.touched());
                source.stop().await
            }
            None => Ok(()),
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

    /// A snapshot of the live volume while the guest runs, for a live
    /// move to carry before the pause. It is independent of the
    /// transient snapshot [`Self::take`] makes, so the capture can take
    /// its own beside it. A precopy left by an earlier move is replaced.
    ///
    /// `Unsupported` for a role with no checkpoint volume: a hydrating
    /// device, or a settled one before its first checkpoint. The blocks
    /// it changed since its manifest are the whole copy plus the guest's
    /// writes, which is not a live move's small difference; the session
    /// moves by snapshot until a checkpoint makes the normal layout.
    pub async fn take_precopy(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<PendingCheckpoint> {
        let previous = self.checkpoint_of(sandbox, role).await?;
        match self
            .disk
            .open(&checkpoint_name(sandbox, role, previous))
            .await
        {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("{role} disk of {sandbox} has no checkpoint volume yet"),
                ))
            }
            Err(e) => return Err(e),
        }
        let name = precopy_name(sandbox, role);
        self.delete_if_present(&name).await?;
        let live = self.live(sandbox, role).await?;
        let snapshot = checkpoint_split::take(self.disk.as_ref(), &live, &name).await?;
        Ok(PendingCheckpoint {
            sandbox,
            role,
            snapshot,
            previous,
        })
    }

    /// Ascending indices of the `block_bytes` blocks the pending
    /// snapshot changed since the sandbox's checkpoint: what a live
    /// move carries before the pause.
    pub async fn changed_blocks(
        &self,
        pending: &PendingCheckpoint,
        block_bytes: u64,
    ) -> io::Result<Vec<u64>> {
        let previous = self
            .disk
            .open(&checkpoint_name(
                pending.sandbox,
                pending.role,
                pending.previous,
            ))
            .await?;
        self.disk
            .dirty_blocks(&previous, &pending.snapshot, block_bytes)
            .await
    }

    /// Ascending indices of the `block_bytes` blocks that differ between
    /// two pending snapshots of one volume: what a live move carries
    /// under the pause, after the earlier snapshot went ahead.
    pub async fn changed_since(
        &self,
        earlier: &PendingCheckpoint,
        later: &PendingCheckpoint,
        block_bytes: u64,
    ) -> io::Result<Vec<u64>> {
        self.disk
            .dirty_blocks(&earlier.snapshot, &later.snapshot, block_bytes)
            .await
    }

    /// Outside the pause: upload what changed since the last checkpoint,
    /// publish the manifest at the lineage's next free version, and make
    /// the snapshot the sandbox's checkpoint volume.
    ///
    /// A role with no checkpoint volume is on a hydrating device, and
    /// `previous` is its destination's manifest: the upload lays the
    /// blocks that changed since the base over it
    /// ([`checkpoint_split::upload_hydrating`]). While the device still
    /// hydrates, the destination takes the published manifest's name
    /// and the snapshot goes, so the names stay the record and no
    /// checkpoint volume exists before the settle; once settled, the
    /// snapshot becomes the checkpoint volume and the destination and
    /// base names go with the layout they described.
    pub async fn publish(&self, pending: PendingCheckpoint) -> io::Result<ManifestRef> {
        let PendingCheckpoint {
            sandbox,
            role,
            snapshot,
            previous,
        } = pending;
        let previous_manifest = self.manifest(previous).await?;
        let previous_name = checkpoint_name(sandbox, role, previous);
        let (manifest, lazy) = match self.disk.open(&previous_name).await {
            Ok(previous_volume) => {
                let manifest = checkpoint_split::upload(
                    self.disk.as_ref(),
                    self.fill.store(),
                    &snapshot,
                    Some((&previous_volume, &previous_manifest)),
                )
                .await?;
                (manifest, None)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let (dest, base) = self.lazy_layout(sandbox, role).await?;
                if parse(&dest).map(|p| p.kind) != Some(Kind::Dest(previous)) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{dest} is not the destination for {previous}"),
                    ));
                }
                let block_bytes = self.disk.region_bytes().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!("{dest} exists on a disk without a hydrating device"),
                    )
                })?;
                let base_volume = self.disk.open(&base).await?;
                let manifest = checkpoint_split::upload_hydrating(
                    self.disk.as_ref(),
                    &self.fill,
                    &snapshot,
                    &base_volume,
                    &previous_manifest,
                    block_bytes,
                )
                .await?;
                (manifest, Some((dest, base)))
            }
            Err(e) => return Err(e),
        };
        // The store, not the volume name, says which versions exist: a
        // crash after `put_manifest` and before the rename leaves a
        // version the name never reached.
        let latest = self
            .fill
            .store()
            .latest_manifest_version(previous.manifest_id)
            .await
            .map_err(store_err)?
            .unwrap_or(previous.version);
        let next = ManifestRef {
            manifest_id: previous.manifest_id,
            version: latest.max(previous.version) + 1,
        };
        self.fill
            .store()
            .put_manifest(next, &manifest)
            .await
            .map_err(store_err)?;
        let tmp = snapshot.name.clone();
        drop(snapshot);
        let published = checkpoint_name(sandbox, role, next);
        match lazy {
            None => {
                self.disk.rename(&tmp, &published).await?;
                self.disk.delete(&previous_name).await?;
            }
            Some((dest, base)) => match self.disk.hydration(&live_name(sandbox, role)).await? {
                Hydration::Hydrating { .. } => {
                    self.disk
                        .rename(&dest, &dest_name(sandbox, role, next))
                        .await?;
                    self.disk.delete(&tmp).await?;
                }
                Hydration::Settled => {
                    self.disk.rename(&tmp, &published).await?;
                    self.disk.delete(&dest).await?;
                    self.disk.delete(&base).await?;
                }
            },
        }
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
        let names: Vec<(VolumeName, Parsed)> = self
            .disk
            .list()
            .await?
            .into_iter()
            .filter_map(|name| parse(&name).map(|p| (name, p)))
            .filter(|(_, p)| p.sandbox == sandbox)
            .collect();
        self.delete_all(names, &mut report).await;
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
        let mut newest: HashMap<(SandboxId, DiskRole), u64> = Default::default();
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
        let mut unwanted = Vec::new();
        for (name, p) in parsed {
            let wanted = match p.kind {
                Kind::Live | Kind::Transient | Kind::Dest(_) | Kind::Base(_) => {
                    live.contains(&p.sandbox)
                }
                Kind::Checkpoint(m) => m.version == newest[&(p.sandbox, p.role)],
            };
            match wanted {
                true if matches!(p.kind, Kind::Checkpoint(_)) => {
                    if !report.kept.contains(&p.sandbox) {
                        report.kept.push(p.sandbox);
                    }
                }
                true => {}
                false => unwanted.push((name, p)),
            }
        }
        self.delete_all(unwanted, &mut report).await;
        Ok(report)
    }

    /// Delete `names`, each sandbox's live volume first. A live volume
    /// that stays keeps the destination and base under it: the kernel
    /// copies from them and `reattach` rebuilds its plan from them.
    async fn delete_all(&self, mut names: Vec<(VolumeName, Parsed)>, report: &mut Report) {
        live_first(&mut names);
        let mut live_stays = HashSet::new();
        for (name, p) in names {
            if matches!(p.kind, Kind::Dest(_) | Kind::Base(_)) && live_stays.contains(&p.sandbox) {
                continue;
            }
            let deleted = self.delete_into(&name, p.kind, report).await;
            if p.kind == Kind::Live && !deleted {
                live_stays.insert(p.sandbox);
            }
        }
    }

    /// Delete one volume into `report`; whether it is gone. A deleted
    /// live volume's source stops with it. A hydrating volume whose
    /// source this process does not serve is not deleted: the kernel
    /// removes a device only after its reads complete, and nothing
    /// answers them, so the delete would hang (`reattach` first).
    async fn delete_into(&self, name: &VolumeName, kind: Kind, report: &mut Report) -> bool {
        if kind == Kind::Live && !self.serving.lock().unwrap().contains_key(name) {
            match self.disk.hydration(name).await {
                Ok(Hydration::Hydrating { source, .. }) => {
                    report.failed.push((
                        name.clone(),
                        io::Error::new(
                            io::ErrorKind::ResourceBusy,
                            format!(
                                "{name} hydrates from {}, which nothing serves",
                                source.display()
                            ),
                        ),
                    ));
                    return false;
                }
                Ok(Hydration::Settled) => {}
                Err(e) => {
                    report.failed.push((name.clone(), e));
                    return false;
                }
            }
        }
        match self.disk.delete(name).await {
            Ok(()) => {
                if kind == Kind::Live {
                    if let Err(e) = self.stop_source(name).await {
                        report.failed.push((name.clone(), e));
                        return true;
                    }
                    self.touched.lock().unwrap().remove(name);
                }
                report.deleted.push(name.clone());
                true
            }
            Err(e) => {
                report.failed.push((name.clone(), e));
                false
            }
        }
    }

    /// The released sandboxes whose kept checkpoints a resume may take.
    fn released(&self) -> Vec<SandboxId> {
        self.kept.lock().unwrap().iter().copied().collect()
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

    /// The manifest the role's live volume holds: its checkpoint
    /// volume's, or, while it hydrates with no checkpoint volume yet,
    /// its destination's.
    pub async fn checkpoint_of(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<ManifestRef> {
        let (mut checkpoint, mut dest): (Option<ManifestRef>, Option<ManifestRef>) = (None, None);
        for p in self.disk.list().await?.iter().filter_map(parse) {
            if (p.sandbox, p.role) != (sandbox, role) {
                continue;
            }
            match p.kind {
                Kind::Checkpoint(m) if checkpoint.is_none_or(|c| m.version > c.version) => {
                    checkpoint = Some(m)
                }
                Kind::Dest(m) => dest = Some(m),
                _ => {}
            }
        }
        checkpoint.or(dest).ok_or_else(|| {
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
    use engram_chunk_store::ChunkStore;
    use engram_storage_local::LocalBlobStorage;

    use super::*;
    use crate::disk::{ChunkIndex, FileDisk};
    use crate::disk_flows::checkpoint;
    use crate::test_support::{CountingBlob, FileSources, HydratingDisk};
    use std::sync::atomic::Ordering;

    const CHUNK: u64 = 4096;

    struct Rig {
        _dir: tempfile::TempDir,
        disk: Arc<dyn SessionDisk>,
        store: ChunkStore,
        volumes: SessionVolumes,
        blob: Arc<CountingBlob>,
        /// Set on a rig whose disk hydrates.
        sources: Option<Arc<FileSources>>,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let disk = Arc::new(FileDisk::new(dir.path().join("volumes"), CHUNK).unwrap());
        rig_on(dir, disk)
    }

    /// A rig whose disk has a hydrating device and whose host serves
    /// sources: resumes take the lazy path.
    fn lazy_rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let disk = Arc::new(HydratingDisk::new(&dir.path().join("volumes"), CHUNK).unwrap());
        let sources = Arc::new(disk.sources(&dir.path().join("sources")));
        let mut r = rig_on(dir, disk);
        r.volumes = r.volumes.with_sources(sources.clone());
        r.sources = Some(sources);
        r
    }

    /// Drive `settle` until the role's volume is settled; the steps it
    /// took.
    async fn settled(r: &Rig, sandbox: SandboxId, role: DiskRole) -> u64 {
        let mut steps = 0;
        loop {
            steps += 1;
            match r.volumes.settle(sandbox, role).await.unwrap() {
                Hydration::Settled => return steps,
                Hydration::Hydrating { .. } => assert!(steps < 100, "the hydration never ends"),
            }
        }
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
            volumes: SessionVolumes::new(
                disk.clone(),
                engram_fill::FillEngine::new(store.clone(), engram_fill::Config::default()),
            ),
            disk,
            store,
            blob,
            sources: None,
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
        r.volumes.images().reconcile(&[image]).await.unwrap();
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

    /// Fill half `half` (0 or 1) of `chunk` with `byte`.
    async fn write_half(path: &std::path::Path, chunk: u64, half: u64, byte: u8) {
        let size = std::fs::metadata(path).unwrap().len();
        let v = Volume::open(VolumeName::new("probe").unwrap(), path, size, CHUNK).unwrap();
        v.write_range(
            chunk * CHUNK + half * CHUNK / 2,
            vec![byte; (CHUNK / 2) as usize],
        )
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
        for name in [
            transient_name(sandbox, DiskRole::Root),
            precopy_name(sandbox, DiskRole::Root),
        ] {
            assert_eq!(
                parse(&name),
                Some(Parsed {
                    sandbox,
                    role: DiskRole::Root,
                    kind: Kind::Transient
                })
            );
        }
        assert_eq!(
            parse(&dest_name(sandbox, DiskRole::Root, manifest)),
            Some(Parsed {
                sandbox,
                role: DiskRole::Root,
                kind: Kind::Dest(manifest)
            })
        );
        for base in [Some(manifest), None] {
            assert_eq!(
                parse(&base_name(sandbox, DiskRole::Swap, base)),
                Some(Parsed {
                    sandbox,
                    role: DiskRole::Swap,
                    kind: Kind::Base(base)
                })
            );
        }
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

    /// An image with no volume on the host stages one image volume; the
    /// sandbox and every later one are snapshots of it.
    #[tokio::test]
    async fn an_image_source_stages_the_image_volume_once() {
        let r = rig();
        let image = staged_image(&r, &[2, 0, 5]).await;
        r.disk
            .delete(&image_volumes::volume_name(image))
            .await
            .unwrap();

        let first = SandboxId::new();
        let p = r
            .volumes
            .provision(first, Source::Image { manifest: image }, None)
            .await
            .unwrap();
        assert_eq!(bytes_at(&p.root).await, vec![2, 0, 5]);
        assert_ne!(p.root_manifest, image);
        r.disk
            .open(&image_volumes::volume_name(image))
            .await
            .expect("the image volume stays");

        let gets = r.blob.gets();
        let second = SandboxId::new();
        let p = r
            .volumes
            .provision(second, Source::Image { manifest: image }, None)
            .await
            .unwrap();
        assert_eq!(bytes_at(&p.root).await, vec![2, 0, 5]);
        assert_eq!(r.blob.gets(), gets, "the second sandbox is a snapshot");
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
                    first_touch: vec![],
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
                    first_touch: vec![],
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(bytes_at(&p.root).await, vec![1, 9]);
        assert_eq!(p.root_manifest, published);
    }

    /// Holds every chunk a stage asks for.
    struct MapPeer(std::collections::HashMap<engram_chunk_store::ChunkHash, bytes::Bytes>);

    #[async_trait::async_trait]
    impl PeerSource for MapPeer {
        async fn fetch(&self, hash: engram_chunk_store::ChunkHash) -> Option<bytes::Bytes> {
            self.0.get(&hash).cloned()
        }
    }

    #[tokio::test]
    async fn a_stage_with_a_peer_reads_the_peer_before_the_store() {
        let r = rig();
        let image = staged_image(&r, &[1, 2]).await;
        let first = SandboxId::new();
        let p = r.volumes.provision(first, fork(image), None).await.unwrap();
        write_byte(&p.root, 1, 9).await;
        let published = r.volumes.checkpoint(first, DiskRole::Root).await.unwrap();
        r.volumes.purge(first).await.unwrap();
        let pattern = [1u8, 9];
        let chunks = r.store.get_manifest(published).await.unwrap().chunks;
        let peer = MapPeer(
            chunks
                .iter()
                .map(|c| {
                    let byte = pattern[(c.offset / CHUNK) as usize];
                    (c.hash, bytes::Bytes::from(vec![byte; CHUNK as usize]))
                })
                .collect(),
        );

        let gets = r.blob.gets();
        let second = SandboxId::new();
        let p = r
            .volumes
            .provision_with_peer(
                second,
                Source::Continue {
                    manifest: published,
                    base: None,
                    first_touch: vec![],
                },
                None,
                Some(Arc::new(peer)),
            )
            .await
            .unwrap();
        assert_eq!(bytes_at(&p.root).await, vec![1, 9]);
        assert_eq!(r.blob.gets(), gets, "every chunk came from the peer");
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
                    first_touch: vec![],
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
                    first_touch: vec![],
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
    async fn a_precopy_snapshot_lives_beside_the_one_the_capture_takes() {
        let r = rig();
        let image = staged_image(&r, &[1, 2, 3, 4]).await;
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
        let half = CHUNK / 2;

        // The move starts: the guest has changed the second half of chunk 1.
        write_half(&p.root, 1, 1, 9).await;
        let precopy = r
            .volumes
            .take_precopy(sandbox, DiskRole::Root)
            .await
            .unwrap();
        assert_eq!(
            r.volumes.changed_blocks(&precopy, half).await.unwrap(),
            vec![3]
        );

        // The guest runs on and changes chunk 3; the capture's snapshot
        // sees only that, and both snapshots stay readable.
        write_half(&p.root, 3, 0, 7).await;
        let sealed = r.volumes.take(sandbox, DiskRole::Root).await.unwrap();
        assert_eq!(
            r.volumes
                .changed_since(&precopy, &sealed, half)
                .await
                .unwrap(),
            vec![6]
        );
        assert_eq!(
            precopy.snapshot().read_range(3 * CHUNK, 1).await.unwrap(),
            vec![4]
        );
        assert_eq!(
            sealed.snapshot().read_range(3 * CHUNK, 1).await.unwrap(),
            vec![7]
        );

        // A second move replaces the precopy; the capture's snapshot is untouched.
        let again = r
            .volumes
            .take_precopy(sandbox, DiskRole::Root)
            .await
            .unwrap();
        assert_eq!(
            again.snapshot().read_range(3 * CHUNK, 1).await.unwrap(),
            vec![7]
        );
        r.volumes.pending(sandbox, DiskRole::Root).await.unwrap();
        r.volumes.discard(again).await.unwrap();
        r.volumes.discard(sealed).await.unwrap();
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
            r.volumes.changed_blocks(&taken, CHUNK).await.unwrap(),
            vec![2]
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

        // A resume runs in a new sandbox.
        let resumed = SandboxId::new();
        let gets = r.blob.gets();
        let p = r
            .volumes
            .provision(
                resumed,
                Source::Continue {
                    manifest: published,
                    base: Some(image),
                    first_touch: vec![],
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.blob.gets(), gets, "no chunk is fetched");
        assert_eq!(bytes_at(&p.root).await, vec![1, 9]);
        assert_eq!(p.root_manifest, published);
        assert_eq!(
            checkpoints_of(&r, resumed, DiskRole::Root).await,
            vec![published]
        );
        assert!(
            checkpoints_of(&r, sandbox, DiskRole::Root).await.is_empty(),
            "the old sandbox's kept checkpoint is retired by the new one"
        );
    }

    #[tokio::test]
    async fn a_live_sandbox_keeps_its_checkpoint_when_another_continues_its_lineage() {
        let r = rig();
        let image = staged_image(&r, &[1, 2]).await;
        let source = SandboxId::new();
        let p = r
            .volumes
            .provision(source, fork(image), None)
            .await
            .unwrap();
        write_byte(&p.root, 1, 9).await;
        let published = r.volumes.checkpoint(source, DiskRole::Root).await.unwrap();

        // A same-host move: the destination continues the lineage while
        // the source still runs.
        let destination = SandboxId::new();
        let p = r
            .volumes
            .provision(
                destination,
                Source::Continue {
                    manifest: published,
                    base: Some(image),
                    first_touch: vec![],
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(bytes_at(&p.root).await, vec![1, 9]);
        assert_eq!(
            checkpoints_of(&r, source, DiskRole::Root).await,
            vec![published],
            "the live source keeps its checkpoint"
        );
        assert_eq!(
            checkpoints_of(&r, destination, DiskRole::Root).await,
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

        let resumed = SandboxId::new();
        let gets = r.blob.gets();
        let p = r
            .volumes
            .provision(
                resumed,
                Source::Continue {
                    manifest: next,
                    base: Some(image),
                    first_touch: vec![],
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.blob.gets(), gets + 1, "only the changed chunk is fetched");
        assert_eq!(bytes_at(&p.root).await, vec![7, 2]);
        assert_eq!(
            checkpoints_of(&r, resumed, DiskRole::Root).await,
            vec![next]
        );
        assert!(checkpoints_of(&r, sandbox, DiskRole::Root).await.is_empty());
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
    async fn a_resume_takes_the_sandbox_out_of_the_reap_queue() {
        let r = pressured_rig();
        let image = staged_image(&r, &[1]).await;
        let sandbox = SandboxId::new();
        r.volumes
            .provision(sandbox, fork(image), None)
            .await
            .unwrap();
        let published = r.volumes.checkpoint(sandbox, DiskRole::Root).await.unwrap();
        r.volumes.release(sandbox).await.unwrap();
        r.volumes
            .provision(
                sandbox,
                Source::Continue {
                    manifest: published,
                    base: Some(image),
                    first_touch: vec![],
                },
                None,
            )
            .await
            .unwrap();

        let report = r.volumes.reap(POOL_TOTAL).await.unwrap();
        assert!(
            report.deleted.is_empty() && report.failed.is_empty(),
            "a live sandbox is never reaped: {report:?}"
        );
        assert_eq!(
            checkpoints_of(&r, sandbox, DiskRole::Root).await,
            vec![published]
        );
    }

    #[tokio::test]
    async fn reconcile_queues_dead_sandboxes_for_the_reap() {
        let r = pressured_rig();
        let image = staged_image(&r, &[1]).await;
        let dead = SandboxId::new();
        r.volumes.provision(dead, fork(image), None).await.unwrap();

        let restarted = SessionVolumes::new(
            r.disk.clone(),
            engram_fill::FillEngine::new(r.store.clone(), engram_fill::Config::default()),
        );
        let report = restarted.reconcile(&[]).await.unwrap();
        assert_eq!(report.kept, vec![dead]);
        assert_eq!(checkpoints_of(&r, dead, DiskRole::Root).await.len(), 1);

        let reaped = restarted.reap(POOL_TOTAL).await.unwrap();
        assert_eq!(reaped.deleted.len(), 1);
        assert!(checkpoints_of(&r, dead, DiskRole::Root).await.is_empty());
    }

    /// Image [1, 2, 3]; the session changed chunk 1 and zeroed chunk 2.
    /// Its resume on a host that hydrates starts on a device over a
    /// snapshot of the image volume: chunk 1 is the whole delta, chunk 2
    /// is zeroed before the copy, chunk 0 is never touched.
    #[tokio::test]
    async fn a_lazy_resume_starts_on_a_hydrating_volume_and_settles_onto_it() {
        let r = lazy_rig();
        let image = staged_image(&r, &[1, 2, 3]).await;
        let first = SandboxId::new();
        let p = r.volumes.provision(first, fork(image), None).await.unwrap();
        assert_eq!(p.root_stage, StagePath::StagedSnapshot);
        write_byte(&p.root, 1, 9).await;
        write_byte(&p.root, 2, 0).await;
        let published = r.volumes.checkpoint(first, DiskRole::Root).await.unwrap();
        r.volumes.release(first).await.unwrap();
        r.volumes.purge(first).await.unwrap();

        let resumed = SandboxId::new();
        let gets = r.blob.gets();
        let p = r
            .volumes
            .provision(
                resumed,
                Source::Continue {
                    manifest: published,
                    base: Some(image),
                    first_touch: vec![],
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            (p.root_stage, p.root_manifest),
            (StagePath::Lazy, published)
        );
        assert_eq!(
            r.blob.gets(),
            gets + 1,
            "the source serves only the changed chunk"
        );
        assert_eq!(
            names(&r).await,
            vec![
                image_volumes::volume_name(image).to_string(),
                live_name(resumed, DiskRole::Root).to_string(),
                base_name(resumed, DiskRole::Root, Some(image)).to_string(),
                dest_name(resumed, DiskRole::Root, published).to_string(),
            ]
            .into_iter()
            .map(|n| n.to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
        );
        // The fake device has no lazy read: until the copy lands, the
        // destination shows the base with the dropped chunk zeroed.
        assert_eq!(bytes_at(&p.root).await, vec![1, 2, 0]);
        assert_eq!(
            r.volumes.hydrating(resumed).await.unwrap(),
            vec![DiskRole::Root]
        );

        let steps = settled(&r, resumed, DiskRole::Root).await;
        assert_eq!(
            steps, 1,
            "the one chunk copies on the first step, then the cutover"
        );
        assert_eq!(bytes_at(&p.root).await, vec![1, 9, 0]);
        assert!(r.volumes.hydrating(resumed).await.unwrap().is_empty());
        let sources = r.sources.as_ref().unwrap();
        assert_eq!(sources.stopped.load(Ordering::Relaxed), 1);
        assert_eq!(
            r.volumes.settle(resumed, DiskRole::Root).await.unwrap(),
            Hydration::Settled,
            "settle is idempotent"
        );

        r.volumes.release(resumed).await.unwrap();
        assert_eq!(
            names(&r).await,
            vec![image_volumes::volume_name(image).to_string()],
            "a hydrating role keeps no checkpoint volume"
        );
    }

    /// Base [1, 2, 3, 0, 5], wanted [1, 9, 0, 4, 5]: chunks 1 and 3
    /// copy in, chunk 2 is zeroed. Before the copy the guest writes 7
    /// over the zeroed chunk and 8 over held chunk 4. A checkpoint then
    /// publishes the wanted disk with the writes laid over it, under
    /// the destination's name: no checkpoint volume exists until the
    /// settle. The next checkpoint, after the settle, makes the normal
    /// layout.
    #[tokio::test]
    async fn a_checkpoint_during_hydration_publishes_the_disk_and_keeps_the_names_honest() {
        let r = lazy_rig();
        let image = staged_image(&r, &[1, 2, 3, 0, 5]).await;
        let first = SandboxId::new();
        let p = r.volumes.provision(first, fork(image), None).await.unwrap();
        write_byte(&p.root, 1, 9).await;
        write_byte(&p.root, 2, 0).await;
        write_byte(&p.root, 3, 4).await;
        let wanted = r.volumes.checkpoint(first, DiskRole::Root).await.unwrap();
        r.volumes.release(first).await.unwrap();
        r.volumes.purge(first).await.unwrap();

        let resumed = SandboxId::new();
        let p = r
            .volumes
            .provision(
                resumed,
                Source::Continue {
                    manifest: wanted,
                    base: Some(image),
                    first_touch: vec![],
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(p.root_stage, StagePath::Lazy);
        assert_eq!(
            r.volumes
                .checkpoint_of(resumed, DiskRole::Root)
                .await
                .unwrap(),
            wanted,
            "the live volume holds the destination's manifest"
        );
        assert_eq!(
            r.volumes.first_touch(resumed, DiskRole::Root),
            vec![1, 3],
            "the source reports what the guest read first (the fake: the delta)"
        );
        write_byte(&p.root, 2, 7).await;
        write_byte(&p.root, 4, 8).await;

        let mid = r.volumes.checkpoint(resumed, DiskRole::Root).await.unwrap();
        assert_eq!(
            (mid.manifest_id, mid.version),
            (wanted.manifest_id, wanted.version + 1)
        );
        assert_eq!(manifest_bytes(&r, mid).await, vec![1, 9, 7, 4, 8]);
        let expected: std::collections::BTreeSet<String> = [
            image_volumes::volume_name(image).to_string(),
            live_name(resumed, DiskRole::Root).to_string(),
            base_name(resumed, DiskRole::Root, Some(image)).to_string(),
            dest_name(resumed, DiskRole::Root, mid).to_string(),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            names(&r).await,
            expected.into_iter().collect::<Vec<_>>(),
            "the destination carries the published manifest; no checkpoint volume yet"
        );
        assert_eq!(
            r.volumes
                .checkpoint_of(resumed, DiskRole::Root)
                .await
                .unwrap(),
            mid
        );

        settled(&r, resumed, DiskRole::Root).await;
        assert_eq!(bytes_at(&p.root).await, vec![1, 9, 7, 4, 8]);
        assert_eq!(
            r.volumes.first_touch(resumed, DiskRole::Root),
            vec![1, 3],
            "the record outlives the source until the volume goes"
        );
        let after = r.volumes.checkpoint(resumed, DiskRole::Root).await.unwrap();
        assert_eq!(after.version, mid.version + 1);
        assert_eq!(manifest_bytes(&r, after).await, vec![1, 9, 7, 4, 8]);
        let expected: std::collections::BTreeSet<String> = [
            image_volumes::volume_name(image).to_string(),
            live_name(resumed, DiskRole::Root).to_string(),
            checkpoint_name(resumed, DiskRole::Root, after).to_string(),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            names(&r).await,
            expected.into_iter().collect::<Vec<_>>(),
            "a settled role has the normal layout"
        );
        r.volumes.release(resumed).await.unwrap();
        assert!(r.volumes.first_touch(resumed, DiskRole::Root).is_empty());
    }

    /// A live move carries the blocks changed since the checkpoint
    /// volume; a role without one (hydrating, or settled before its
    /// first checkpoint) refuses the precopy, and the first checkpoint
    /// makes the live move possible again.
    #[tokio::test]
    async fn a_role_without_a_checkpoint_volume_refuses_a_live_move() {
        let r = lazy_rig();
        let image = staged_image(&r, &[1, 2, 3]).await;
        let first = SandboxId::new();
        let p = r.volumes.provision(first, fork(image), None).await.unwrap();
        write_byte(&p.root, 1, 9).await;
        let wanted = r.volumes.checkpoint(first, DiskRole::Root).await.unwrap();
        r.volumes.release(first).await.unwrap();
        r.volumes.purge(first).await.unwrap();

        let resumed = SandboxId::new();
        r.volumes
            .provision(
                resumed,
                Source::Continue {
                    manifest: wanted,
                    base: Some(image),
                    first_touch: vec![],
                },
                None,
            )
            .await
            .unwrap();
        let refused = |e: io::Error| assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{e}");
        refused(
            r.volumes
                .take_precopy(resumed, DiskRole::Root)
                .await
                .unwrap_err(),
        );
        settled(&r, resumed, DiskRole::Root).await;
        refused(
            r.volumes
                .take_precopy(resumed, DiskRole::Root)
                .await
                .unwrap_err(),
        );
        r.volumes.checkpoint(resumed, DiskRole::Root).await.unwrap();
        let precopy = r
            .volumes
            .take_precopy(resumed, DiskRole::Root)
            .await
            .unwrap();
        assert!(r
            .volumes
            .changed_blocks(&precopy, CHUNK)
            .await
            .unwrap()
            .is_empty());
        r.volumes.discard(precopy).await.unwrap();
        r.volumes.release(resumed).await.unwrap();
    }

    /// The first byte of each chunk of a published manifest; 0 for a
    /// hole.
    async fn manifest_bytes(r: &Rig, manifest: ManifestRef) -> Vec<u8> {
        let fill = engram_fill::FillEngine::new(r.store.clone(), engram_fill::Config::default());
        let manifest = r.store.get_manifest(manifest).await.unwrap();
        let mut out = Vec::new();
        for i in 0..manifest.expected_chunk_count() {
            out.push(match manifest.chunk_at(i * CHUNK) {
                Some(c) => fill
                    .read(c.hash, engram_fill::Class::Hydrate)
                    .await
                    .unwrap()[0],
                None => 0,
            });
        }
        out
    }

    /// A release during the window deletes the device before its
    /// source; the source refuses the other order.
    #[tokio::test]
    async fn a_release_during_hydration_ends_the_device_then_its_source() {
        let r = lazy_rig();
        let image = staged_image(&r, &[1, 2]).await;
        let sandbox = SandboxId::new();
        let mut newer = r.store.get_manifest(image).await.unwrap();
        newer.chunks[0].hash = r.store.put_chunk(&vec![7u8; CHUNK as usize]).await.unwrap();
        let next = ManifestRef::new();
        r.store.put_manifest(next, &newer).await.unwrap();
        let p = r
            .volumes
            .provision(
                sandbox,
                Source::Continue {
                    manifest: next,
                    base: Some(image),
                    first_touch: vec![],
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(p.root_stage, StagePath::Lazy);
        assert_eq!(
            r.volumes.hydrating(sandbox).await.unwrap(),
            vec![DiskRole::Root]
        );

        r.volumes.release(sandbox).await.unwrap();
        let sources = r.sources.as_ref().unwrap();
        assert_eq!(sources.stopped.load(Ordering::Relaxed), 1);
        assert_eq!(
            names(&r).await,
            vec![image_volumes::volume_name(image).to_string()]
        );
    }

    /// A host-agent restart mid-window: the device and its source
    /// outlive the process; the next process cannot delete the device
    /// before it serves the source again, then settles the volume.
    #[tokio::test]
    async fn a_restart_serves_a_hydrating_volume_again() {
        let r = lazy_rig();
        let image = staged_image(&r, &[1, 2]).await;
        let sandbox = SandboxId::new();
        let mut newer = r.store.get_manifest(image).await.unwrap();
        newer.chunks[0].hash = r.store.put_chunk(&vec![7u8; CHUNK as usize]).await.unwrap();
        let next = ManifestRef::new();
        r.store.put_manifest(next, &newer).await.unwrap();
        let p = r
            .volumes
            .provision(
                sandbox,
                Source::Continue {
                    manifest: next,
                    base: Some(image),
                    first_touch: vec![],
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(p.root_stage, StagePath::Lazy);

        let sources = r.sources.clone().unwrap();
        let restarted = SessionVolumes::new(
            r.disk.clone(),
            engram_fill::FillEngine::new(r.store.clone(), engram_fill::Config::default()),
        )
        .with_sources(sources.clone());
        // Until the source is served again, nothing deletes the device:
        // its removal would wait on reads nothing answers.
        let refused = restarted.release(sandbox).await.unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::ResourceBusy);
        assert_eq!(
            restarted.reattach(sandbox).await.unwrap(),
            vec![DiskRole::Root]
        );
        assert!(
            restarted.reattach_all().await.unwrap().is_empty(),
            "a role this process serves is not served twice"
        );
        loop {
            if let Hydration::Settled = restarted.settle(sandbox, DiskRole::Root).await.unwrap() {
                break;
            }
        }
        assert_eq!(bytes_at(&p.root).await, vec![7, 2]);
        assert_eq!(sources.stopped.load(Ordering::Relaxed), 1);
        assert!(restarted.reattach(sandbox).await.unwrap().is_empty());
    }

    /// Without a base on the host, the destination starts from zeros and
    /// every listed chunk is the delta.
    #[tokio::test]
    async fn a_lazy_resume_without_a_base_starts_from_zeros() {
        let r = lazy_rig();
        let image = staged_image(&r, &[1, 0, 3]).await;
        let sandbox = SandboxId::new();
        let p = r
            .volumes
            .provision(
                sandbox,
                Source::Continue {
                    manifest: image,
                    base: None,
                    first_touch: vec![],
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(p.root_stage, StagePath::Lazy);
        assert_eq!(bytes_at(&p.root).await, vec![0, 0, 0]);
        assert_eq!(
            settled(&r, sandbox, DiskRole::Root).await,
            2,
            "two chunks to copy"
        );
        assert_eq!(bytes_at(&p.root).await, vec![1, 0, 3]);
        assert!(names(&r)
            .await
            .contains(&base_name(sandbox, DiskRole::Root, None).to_string()));
    }
}
