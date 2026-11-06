//! A session's disks as volumes. Per role (root, swap) a sandbox has a
//! live volume the hypervisor opens and one checkpoint volume. The
//! names are the only record:
//!
//! ```text
//! sb-<sandbox>-<role>                        live
//! sb-<sandbox>-<role>-ckpt-<manifest>-v<N>   the last checkpoint; holds that manifest
//! tmp-sb-<sandbox>-<role>                    a stage or a checkpoint in flight
//! tmp-sb-<sandbox>-<role>-pre                a live move's snapshot while the guest runs
//! ```
//!
//! A role restored before its bytes are local is a cloned drive (ADR
//! 0127). Its live name is then a drive, not a volume, over these:
//!
//! ```text
//! sb-<sandbox>-<role>-dest-<manifest>-v<N>   the destination; holds the lineage like a checkpoint
//! sb-<sandbox>-<role>-bitmap                 the drive's copy bitmap
//! tmp-sb-<sandbox>-<role>-bm                 the bitmap snapshot of a checkpoint in flight
//! tmp-sb-<sandbox>-<role>-view               a drive that reads that checkpoint
//! ```
//!
//! A lazy provision makes a cloned role over a source from
//! [`DiskSources`]. A checkpoint of one reads through a view until the
//! cutover ([`SessionVolumes::cut_over`]), which also deletes the bitmap
//! and stops the source; the first checkpoint after it is a `-ckpt-`
//! volume, and from then on the role checkpoints as any other. A live
//! move of a cloned role is refused.
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

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use engram_chunk_store::{ChunkSize, ChunkStore, Manifest, ManifestKind};
use engram_core::types::ids::SandboxId;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::snapshot::DiskRole;
use uuid::Uuid;

use crate::disk::{
    clone_bitmap_bytes, Capacity, ChunkIndex, CloneState, Hydration, SessionDisk, Volume,
    VolumeName, CLONE_REGION_BYTES,
};
use crate::disk_flows::{checkpoint_split, shape_matches, stage};
use crate::disk_sources::DiskSources;
use crate::image_volumes;

const LIVE_PREFIX: &str = "sb-";
const TRANSIENT_PREFIX: &str = "tmp-sb-";
const CHECKPOINT_INFIX: &str = "ckpt-";
const DEST_INFIX: &str = "dest-";
const BITMAP_SUFFIX: &str = "bitmap";
const PRECOPY_SUFFIX: &str = "pre";
const BITMAP_SNAPSHOT_SUFFIX: &str = "bm";
const VIEW_SUFFIX: &str = "view";

/// What a volume name says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Live,
    Checkpoint(ManifestRef),
    Transient,
    Dest(ManifestRef),
    Bitmap,
    BitmapSnapshot,
    View,
}

impl Kind {
    /// A volume or drive that only a cloned role has.
    fn is_clone_part(self) -> bool {
        match self {
            Self::Dest(_) | Self::Bitmap | Self::BitmapSnapshot | Self::View => true,
            Self::Live | Self::Checkpoint(_) | Self::Transient => false,
        }
    }

    /// The manifest a volume that holds the role's lineage names, and
    /// which kind of holder it is.
    fn holder(self) -> Option<(ManifestRef, Holder)> {
        match self {
            Self::Checkpoint(m) => Some((m, Holder::Checkpoint)),
            Self::Dest(m) => Some((m, Holder::Dest)),
            Self::Live | Self::Transient | Self::Bitmap | Self::BitmapSnapshot | Self::View => None,
        }
    }
}

/// The volume that holds a role's lineage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Holder {
    /// A complete snapshot.
    Checkpoint,
    /// A cloned drive's destination, which is not complete before the
    /// cutover.
    Dest,
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

/// `<prefix><sandbox>-<role>[-<suffix>]`. The longest, a checkpoint or
/// destination name with a 20-digit version, is 100 bytes: that leaves
/// 27 of device-mapper's 128 for a pool's prefix on the device name.
fn role_name(prefix: &str, sandbox: SandboxId, role: DiskRole, suffix: Option<&str>) -> VolumeName {
    let sandbox = sandbox.as_uuid().simple();
    let name = match suffix {
        Some(suffix) => format!("{prefix}{sandbox}-{role}-{suffix}"),
        None => format!("{prefix}{sandbox}-{role}"),
    };
    VolumeName::new(name).expect("a session volume name is at most 100 bytes of [a-z0-9-]")
}

fn manifest_suffix(infix: &str, manifest: ManifestRef) -> String {
    format!(
        "{infix}{}-v{}",
        manifest.manifest_id.simple(),
        manifest.version
    )
}

pub fn live_name(sandbox: SandboxId, role: DiskRole) -> VolumeName {
    role_name(LIVE_PREFIX, sandbox, role, None)
}

pub fn checkpoint_name(sandbox: SandboxId, role: DiskRole, manifest: ManifestRef) -> VolumeName {
    role_name(
        LIVE_PREFIX,
        sandbox,
        role,
        Some(&manifest_suffix(CHECKPOINT_INFIX, manifest)),
    )
}

/// The volume a cloned drive writes into, named after its lineage's
/// newest manifest.
pub fn dest_name(sandbox: SandboxId, role: DiskRole, manifest: ManifestRef) -> VolumeName {
    role_name(
        LIVE_PREFIX,
        sandbox,
        role,
        Some(&manifest_suffix(DEST_INFIX, manifest)),
    )
}

pub fn bitmap_name(sandbox: SandboxId, role: DiskRole) -> VolumeName {
    role_name(LIVE_PREFIX, sandbox, role, Some(BITMAP_SUFFIX))
}

fn transient_name(sandbox: SandboxId, role: DiskRole) -> VolumeName {
    role_name(TRANSIENT_PREFIX, sandbox, role, None)
}

/// The snapshot a live move takes while the guest runs; it lives
/// beside the transient one the capture takes under the pause.
fn precopy_name(sandbox: SandboxId, role: DiskRole) -> VolumeName {
    role_name(TRANSIENT_PREFIX, sandbox, role, Some(PRECOPY_SUFFIX))
}

/// The bitmap half of a cloned drive's checkpoint; the transient
/// volume is the destination half.
pub fn bitmap_snapshot_name(sandbox: SandboxId, role: DiskRole) -> VolumeName {
    role_name(
        TRANSIENT_PREFIX,
        sandbox,
        role,
        Some(BITMAP_SNAPSHOT_SUFFIX),
    )
}

/// The read-only drive over a cloned drive's checkpoint.
pub fn view_name(sandbox: SandboxId, role: DiskRole) -> VolumeName {
    role_name(TRANSIENT_PREFIX, sandbox, role, Some(VIEW_SUFFIX))
}

/// What `name` says; `None` for a volume that is not a session's.
pub fn parse(name: &VolumeName) -> Option<Parsed> {
    let (rest, transient) = match name.as_str().strip_prefix(TRANSIENT_PREFIX) {
        Some(rest) => (rest, true),
        None => (name.as_str().strip_prefix(LIVE_PREFIX)?, false),
    };
    let (sandbox, rest) = rest.split_once('-')?;
    let sandbox = SandboxId::from(Uuid::try_parse(sandbox).ok()?);
    let (role, suffix) = match rest.split_once('-') {
        Some((role, suffix)) => (role, Some(suffix)),
        None => (rest, None),
    };
    let role: DiskRole = role.parse().ok()?;
    let manifest = |text: &str| {
        let (id, version) = text.split_once("-v")?;
        Some(ManifestRef {
            manifest_id: Uuid::try_parse(id).ok()?,
            version: version.parse().ok()?,
        })
    };
    let kind = match (transient, suffix) {
        (false, None) => Kind::Live,
        (false, Some(BITMAP_SUFFIX)) => Kind::Bitmap,
        (false, Some(suffix)) => match (
            suffix.strip_prefix(CHECKPOINT_INFIX),
            suffix.strip_prefix(DEST_INFIX),
        ) {
            (Some(m), _) => Kind::Checkpoint(manifest(m)?),
            (_, Some(m)) => Kind::Dest(manifest(m)?),
            (None, None) => return None,
        },
        (true, None | Some(PRECOPY_SUFFIX)) => Kind::Transient,
        (true, Some(BITMAP_SNAPSHOT_SUFFIX)) => Kind::BitmapSnapshot,
        (true, Some(VIEW_SUFFIX)) => Kind::View,
        (true, Some(_)) => return None,
    };
    Some(Parsed {
        sandbox,
        role,
        kind,
    })
}

/// The role has a volume or drive that only a cloned role has.
fn cloned_in(names: &[VolumeName], sandbox: SandboxId, role: DiskRole) -> bool {
    names
        .iter()
        .filter_map(parse)
        .any(|p| p.sandbox == sandbox && p.role == role && p.kind.is_clone_part())
}

/// The roles among `parsed` that are cloned, each once.
fn cloned_roles<'a>(parsed: impl IntoIterator<Item = &'a Parsed>) -> Vec<(SandboxId, DiskRole)> {
    let mut roles = Vec::new();
    for p in parsed {
        if p.kind.is_clone_part() && !roles.contains(&(p.sandbox, p.role)) {
            roles.push((p.sandbox, p.role));
        }
    }
    roles
}

/// A role's newest lineage holder, or the manifest ids of two lineages.
type Lineage = Result<(ManifestRef, Holder), [Uuid; 2]>;

/// The lineage of each role among `parsed`. Two lineages for one role
/// is an error: the names are the only record, so nothing says which
/// one is the session's.
fn holders<'a>(
    parsed: impl IntoIterator<Item = &'a Parsed>,
) -> HashMap<(SandboxId, DiskRole), Lineage> {
    let mut holders = HashMap::new();
    for p in parsed {
        let Some((m, holder)) = p.kind.holder() else {
            continue;
        };
        let entry = holders
            .entry((p.sandbox, p.role))
            .or_insert(Ok((m, holder)));
        *entry = match *entry {
            Ok((newest, _)) if newest.manifest_id != m.manifest_id => {
                Err([newest.manifest_id, m.manifest_id])
            }
            Ok((newest, _)) if m.version > newest.version => Ok((m, holder)),
            kept => kept,
        };
    }
    holders
}

fn two_lineages(sandbox: SandboxId, role: DiskRole, ids: [Uuid; 2]) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "sandbox {sandbox} has two {role} lineages, {} and {}",
            ids[0], ids[1]
        ),
    )
}

/// The manifest the role's lineage holds and its holder: the newest of
/// its checkpoint volumes and destinations.
fn lineage_in(
    names: &[VolumeName],
    sandbox: SandboxId,
    role: DiskRole,
) -> io::Result<(ManifestRef, Holder)> {
    let parsed: Vec<Parsed> = names
        .iter()
        .filter_map(parse)
        .filter(|p| p.sandbox == sandbox && p.role == role)
        .collect();
    match holders(&parsed).remove(&(sandbox, role)) {
        Some(holder) => holder.map_err(|ids| two_lineages(sandbox, role, ids)),
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("sandbox {sandbox} has no {role} checkpoint volume"),
        )),
    }
}

/// The name of the role's destination.
fn dest_in(names: &[VolumeName], sandbox: SandboxId, role: DiskRole) -> io::Result<VolumeName> {
    names
        .iter()
        .find(|name| {
            parse(name).is_some_and(|p| {
                p.sandbox == sandbox && p.role == role && matches!(p.kind, Kind::Dest(_))
            })
        })
        .cloned()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("sandbox {sandbox} has no {role} destination"),
            )
        })
}

/// The refusal of `op` on a cloned role.
fn cloned_refused(sandbox: SandboxId, role: DiskRole, op: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("the {role} disk of sandbox {sandbox} is a cloned drive, which cannot {op}"),
    )
}

/// `e`, from the source of the role, with the role named.
fn source_err(sandbox: SandboxId, role: DiskRole, e: io::Error) -> io::Error {
    let what = format!("source of the {role} disk of sandbox {sandbox}: {e}");
    io::Error::new(e.kind(), what)
}

/// Where a live volume's contents come from. A manifest is written in
/// full before the sandbox starts; when `base` names an image staged on
/// this host, that volume is the origin and only the difference is
/// written. A manifest that is itself a staged image is snapshotted.
///
/// A lazy source makes the role a cloned drive instead: the sandbox
/// starts at once, reads what is not yet local from a source that serves
/// the manifest, and the drive copies the rest in the background.
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
    /// [`Self::Fork`] over a cloned drive.
    LazyFork {
        manifest: ManifestRef,
        base: Option<ManifestRef>,
    },
    /// [`Self::Continue`] over a cloned drive.
    LazyContinue {
        manifest: ManifestRef,
        base: Option<ManifestRef>,
    },
}

impl Source {
    /// The lazy counterpart: a fork or a continue over a cloned drive.
    /// Zeros stay zeros.
    pub fn into_lazy(self) -> Self {
        match self {
            Self::Fork { manifest, base } => Self::LazyFork { manifest, base },
            Self::Continue { manifest, base } => Self::LazyContinue { manifest, base },
            other => other,
        }
    }
}

/// The byte ranges of a cloned drive whose destination already holds
/// `wanted` there, from the manifests alone. `origin` is the manifest the
/// destination holds (a snapshot of a staged volume), `None` for a new
/// volume, which reads as zeros. A chunk qualifies when both list the
/// same hash or neither lists it. Each run of such chunks is rounded
/// inward to whole regions, the unit a skip marks.
fn hydration_skips(wanted: &Manifest, origin: Option<&Manifest>) -> io::Result<Vec<Range<u64>>> {
    let zeros = Manifest {
        chunk_size: wanted.chunk_size,
        ..Manifest::empty(ManifestKind::Disk, wanted.total_bytes)
    };
    let delta = wanted
        .delta_from(origin.unwrap_or(&zeros))
        .map_err(store_err)?;
    let differs: HashSet<u64> = delta
        .write
        .iter()
        .map(|c| c.offset)
        .chain(delta.zero)
        .collect();
    let chunk = wanted.chunk_size.as_u64();
    let mut runs: Vec<Range<u64>> = Vec::new();
    for offset in (0..wanted.expected_chunk_count()).map(|i| i * chunk) {
        if differs.contains(&offset) {
            continue;
        }
        let end = (offset + chunk).min(wanted.total_bytes);
        match runs.last_mut() {
            Some(run) if run.end == offset => run.end = end,
            _ => runs.push(offset..end),
        }
    }
    Ok(runs
        .into_iter()
        .map(|run| {
            run.start.next_multiple_of(CLONE_REGION_BYTES)
                ..run.end / CLONE_REGION_BYTES * CLONE_REGION_BYTES
        })
        .filter(|run| run.start < run.end)
        .collect())
}

/// The writes of an eager provision: `wanted`, as its difference from
/// `base` when the provision starts from a volume that holds it.
#[derive(Clone, Debug)]
pub struct EagerPlan {
    pub wanted: Manifest,
    pub base: Option<Manifest>,
}

/// Where an eager provision starts.
enum Origin {
    /// The staged image volume that holds `wanted`, to snapshot.
    Staged { volume: Volume, wanted: Manifest },
    /// The checkpoint `owner` kept at the manifest, to adopt.
    Kept { owner: SandboxId, volume: Volume },
    /// `wanted` staged into a new volume, onto `base` (a volume and the
    /// manifest it holds) when there is one.
    Stage {
        wanted: Manifest,
        base: Option<(Volume, Manifest)>,
    },
}

/// The kernel's default copy depth for a cloned drive. The drive is made
/// with hydration off, so it copies nothing before its skips are marked.
const LAZY_HYDRATION: Hydration = Hydration {
    enabled: false,
    threshold_regions: 1,
    batch_regions: 1,
};

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
/// it leaves the transient volumes for the next `take`, `pending` or
/// `release` to find.
#[derive(Debug)]
pub struct PendingCheckpoint {
    sandbox: SandboxId,
    role: DiskRole,
    snapshot: Volume,
    previous: ManifestRef,
    shape: Shape,
}

/// What a pending checkpoint snapshotted, which says how it publishes.
#[derive(Debug)]
enum Shape {
    /// A volume next to the checkpoint volume of `previous`: the upload
    /// is the difference from that volume.
    Volume,
    /// A cloned drive's destination with this bitmap snapshot, taken
    /// together before the cutover. The destination snapshot reads zeros
    /// where the drive had not copied, so the upload reads through a view.
    Hydrating { bitmap: Volume },
    /// A cloned drive's destination after the cutover, with no complete
    /// volume of `previous`: the upload hashes every chunk.
    CutOver,
}

/// The regions a cloned drive has copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Copied {
    pub hydrated_regions: u64,
    pub total_regions: u64,
}

/// Where a cloned role is on its way to a finished cutover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hydrating {
    /// Regions are still to copy.
    Copying(Copied),
    /// Every region is copied, or the drive is cut over and its bitmap
    /// or source is left: [`SessionVolumes::cut_over`] finishes it.
    Ready,
}

/// What [`SessionVolumes::cut_over`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cutover {
    /// The drive maps its destination alone, and the bitmap and the
    /// source are gone.
    Done,
    /// Nothing changed; the cutover can go ahead later.
    Waiting(Waiting),
}

/// Why a cutover waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waiting {
    /// Regions are still to copy.
    Copying(Copied),
    /// A checkpoint of the role is in flight, or a crash left one: its
    /// publish reads through a view over the source.
    Checkpoint,
}

/// Where a role's writes are.
enum Written {
    /// Its live volume.
    Live,
    /// The destination, named here, under a cut-over drive.
    CutOver(VolumeName),
    /// The destination, named here, under a drive that copies.
    Copying(VolumeName),
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
    /// The volumes deleted and the drives removed.
    pub deleted: Vec<VolumeName>,
    pub failed: Vec<(VolumeName, io::Error)>,
    /// Sandboxes that still have a checkpoint volume.
    pub kept: Vec<SandboxId>,
}

/// The owner of every `sb-` volume on a host.
pub struct SessionVolumes {
    disk: Arc<dyn SessionDisk>,
    store: ChunkStore,
    /// `None` refuses a lazy source.
    sources: Option<Arc<dyn DiskSources>>,
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
            sources: None,
            kept: Mutex::new(VecDeque::new()),
        }
    }

    pub fn with_sources(mut self, sources: Arc<dyn DiskSources>) -> Self {
        self.sources = Some(sources);
        self
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
        // A sandbox resumed on this host is live again: its kept
        // checkpoint is no longer the reap's to purge.
        self.kept.lock().unwrap().retain(|s| *s != sandbox);
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

    /// Eager: the checkpoint volume first, under the transient name until
    /// its manifest is in the store; then the live volume as its
    /// snapshot. A `Continue` of a checkpoint this host kept for the
    /// lineage skips the stage; one of a newer manifest stages only the
    /// delta onto it. Lazy: a cloned drive, see [`Self::clone_role`].
    async fn provision_role(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        source: Source,
    ) -> io::Result<(PathBuf, ManifestRef)> {
        let tmp = transient_name(sandbox, role);
        self.delete_if_present(&tmp).await?;
        Ok(match source {
            Source::Fork { manifest, base } | Source::Continue { manifest, base } => {
                let continues = matches!(source, Source::Continue { .. });
                let wanted = match self
                    .eager_origin(sandbox, role, manifest, base, continues)
                    .await?
                {
                    Origin::Kept { owner, volume } => {
                        let checkpoint = self.adopt(owner, volume, sandbox, role, manifest).await?;
                        return Ok((self.live_over(&checkpoint, sandbox, role).await?, manifest));
                    }
                    Origin::Staged { volume, wanted } => {
                        self.disk.snapshot(&volume, &tmp).await?;
                        wanted
                    }
                    Origin::Stage { wanted, base } => {
                        let base = base.as_ref().map(|(volume, held)| (volume, held));
                        stage(self.disk.as_ref(), &self.store, &tmp, &wanted, base).await?;
                        wanted
                    }
                };
                let lineage = if continues {
                    manifest
                } else {
                    self.new_lineage(&wanted).await?
                };
                let checkpoint = self.commit(&tmp, sandbox, role, lineage).await?;
                (self.live_over(&checkpoint, sandbox, role).await?, lineage)
            }
            Source::Empty { size_bytes } => {
                let volume = self.disk.create(&tmp, size_bytes).await?;
                let manifest = Manifest {
                    chunk_size: ChunkSize::bytes(volume.chunk_size),
                    ..Manifest::empty(ManifestKind::Disk, size_bytes)
                };
                let lineage = self.new_lineage(&manifest).await?;
                let checkpoint = self.commit(&tmp, sandbox, role, lineage).await?;
                (self.live_over(&checkpoint, sandbox, role).await?, lineage)
            }
            Source::LazyFork { manifest, base } => {
                let wanted = self.store.get_manifest(manifest).await.map_err(store_err)?;
                let lineage = self.new_lineage(&wanted).await?;
                let drive = self
                    .clone_role(sandbox, role, manifest, &wanted, base, lineage)
                    .await?;
                (drive, lineage)
            }
            Source::LazyContinue { manifest, base } => {
                let wanted = self.store.get_manifest(manifest).await.map_err(store_err)?;
                let drive = self
                    .clone_role(sandbox, role, manifest, &wanted, base, manifest)
                    .await?;
                (drive, manifest)
            }
        })
    }

    /// Where an eager provision of `manifest` starts. A fork snapshots the
    /// staged image of the manifest when this host has it; a continue
    /// adopts the checkpoint this host kept at the manifest, or stages the
    /// difference onto a kept checkpoint of the lineage. Otherwise the
    /// manifest is staged onto the staged image of `base`, or in full.
    async fn eager_origin(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        manifest: ManifestRef,
        base: Option<ManifestRef>,
        continues: bool,
    ) -> io::Result<Origin> {
        let kept = if continues {
            self.kept(sandbox, role, manifest).await?
        } else {
            if let Some((volume, wanted)) = self.staged(manifest).await {
                return Ok(Origin::Staged { volume, wanted });
            }
            None
        };
        let kept = match kept {
            Some((owner, volume, _, held)) if held == manifest => {
                return Ok(Origin::Kept { owner, volume })
            }
            kept => kept,
        };
        let wanted = self.store.get_manifest(manifest).await.map_err(store_err)?;
        let base = match kept {
            Some((_, volume, held_manifest, _)) if volume.size_bytes == wanted.total_bytes => {
                Some((volume, held_manifest))
            }
            _ => match base {
                Some(image) => self.staged(image).await,
                None => None,
            },
        };
        Ok(Origin::Stage { wanted, base })
    }

    /// What an eager provision of `source` would write, read from the
    /// manifests and the volume names only. `None` when it writes nothing:
    /// it snapshots a staged image, adopts a kept checkpoint, or makes
    /// zeros. A lazy source is planned as its eager counterpart.
    pub async fn eager_plan(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        source: Source,
    ) -> io::Result<Option<EagerPlan>> {
        let (manifest, base, continues) = match source {
            Source::Fork { manifest, base } | Source::LazyFork { manifest, base } => {
                (manifest, base, false)
            }
            Source::Continue { manifest, base } | Source::LazyContinue { manifest, base } => {
                (manifest, base, true)
            }
            Source::Empty { .. } => return Ok(None),
        };
        Ok(
            match self
                .eager_origin(sandbox, role, manifest, base, continues)
                .await?
            {
                Origin::Stage { wanted, base } => Some(EagerPlan {
                    wanted,
                    base: base.map(|(_, held)| held),
                }),
                Origin::Staged { .. } | Origin::Kept { .. } => None,
            },
        )
    }

    /// A lazy provision can start the source it needs.
    pub fn can_start_sources(&self) -> bool {
        self.sources.as_ref().is_some_and(|s| s.can_start())
    }

    /// A new lineage that holds a copy of `manifest`.
    async fn new_lineage(&self, manifest: &Manifest) -> io::Result<ManifestRef> {
        let lineage = ManifestRef::new();
        self.store
            .put_manifest(lineage, manifest)
            .await
            .map_err(store_err)?;
        Ok(lineage)
    }

    /// The live volume of a role, as a snapshot of its checkpoint.
    async fn live_over(
        &self,
        checkpoint: &Volume,
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<PathBuf> {
        let live = self
            .disk
            .snapshot(checkpoint, &live_name(sandbox, role))
            .await?;
        Ok(live.path)
    }

    /// Make the role a cloned drive that reads `wanted` from a source
    /// serving `served`, and return the drive's path. The destination is
    /// a snapshot of the staged volume of `served` or of `base` when this
    /// host has one, else a new volume; it is named after `lineage`.
    /// Hydration skips the regions the destination already holds as
    /// `wanted` ([`hydration_skips`]). A checkpoint takes the source
    /// manifest's chunk wherever the destination was not written, which
    /// there is the destination's bytes. A failure leaves the rest to the
    /// caller's purge.
    async fn clone_role(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        served: ManifestRef,
        wanted: &Manifest,
        base: Option<ManifestRef>,
        lineage: ManifestRef,
    ) -> io::Result<PathBuf> {
        let sources = self.sources(role)?;
        let bitmap = self
            .disk
            .create(
                &bitmap_name(sandbox, role),
                clone_bitmap_bytes(wanted.total_bytes),
            )
            .await?;
        let staged = match self.staged(served).await {
            Some((volume, held)) => Some((volume, served, held)),
            None => match base {
                Some(image) => self
                    .staged(image)
                    .await
                    .map(|(volume, held)| (volume, image, held)),
                None => None,
            },
        };
        let name = dest_name(sandbox, role, lineage);
        let dest = match &staged {
            Some((origin, _, _)) => self.disk.snapshot(origin, &name).await?,
            None => self.disk.create(&name, wanted.total_bytes).await?,
        };
        shape_matches(&dest, wanted)?;
        let skips = hydration_skips(wanted, staged.as_ref().map(|(_, _, held)| held))?;
        let source = sources
            .start(
                sandbox,
                role,
                served,
                staged.as_ref().map(|(volume, image, _)| (volume, *image)),
            )
            .await?;
        let drive = live_name(sandbox, role);
        let path = self
            .disk
            .clone_drive(&drive, &dest, &bitmap, &source, LAZY_HYDRATION)
            .await?
            .path;
        self.disk.skip_hydration(&drive, &skips).await?;
        self.disk.enable_hydration(&drive).await?;
        Ok(path)
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
        Ok(match self.store.get_manifest(held).await {
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
            self.store.get_manifest(image).await,
        ) {
            (Ok(volume), Ok(manifest)) => Some((volume, manifest)),
            _ => None,
        }
    }

    /// The roles the sandbox has a live volume or a cloned drive for.
    /// [`SessionDisk::list`] does not list drives; a drive holds its
    /// destination, so a cloned role always has a listed volume.
    pub async fn roles(&self, sandbox: SandboxId) -> io::Result<Vec<DiskRole>> {
        let names = self.disk.list().await?;
        Ok(DiskRole::ALL
            .into_iter()
            .filter(|role| {
                names.contains(&live_name(sandbox, *role)) || cloned_in(&names, sandbox, *role)
            })
            .collect())
    }

    /// A live volume, the one the hypervisor opens: for a cloned role,
    /// the drive once it is cut over. `Unsupported` while it copies.
    pub async fn live(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<Volume> {
        let name = live_name(sandbox, role);
        match self.disk.open(&name).await {
            Err(_) if self.cloned(sandbox, role).await? => {
                match self.disk.clone_state(&name).await? {
                    CloneState::CutOver => self.disk.open_clone(&name).await,
                    _ => Err(cloned_refused(
                        sandbox,
                        role,
                        "open as a volume while it copies",
                    )),
                }
            }
            opened => opened,
        }
    }

    /// The device path the hypervisor opens: the live volume's, or the
    /// drive's for a cloned role.
    pub async fn path(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<PathBuf> {
        let name = live_name(sandbox, role);
        match self.disk.open(&name).await {
            Err(_) if self.cloned(sandbox, role).await? => {
                Ok(self.disk.open_clone(&name).await?.path)
            }
            opened => Ok(opened?.path),
        }
    }

    async fn cloned(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<bool> {
        Ok(cloned_in(&self.disk.list().await?, sandbox, role))
    }

    /// The role's lineage, its holder, and where its writes are. Only the
    /// kernel knows whether a drive still copies; after its cutover the
    /// role snapshots its destination as a plain role snapshots its live
    /// volume.
    async fn written(
        &self,
        names: &[VolumeName],
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<(ManifestRef, Holder, Written)> {
        let (previous, holder) = lineage_in(names, sandbox, role)?;
        if !cloned_in(names, sandbox, role) {
            return Ok((previous, holder, Written::Live));
        }
        let drive = live_name(sandbox, role);
        let dest = dest_in(names, sandbox, role)?;
        let written = match (self.disk.clone_state(&drive).await?, holder) {
            (CloneState::Copying { .. }, Holder::Dest) => Written::Copying(dest),
            (CloneState::CutOver, _) => Written::CutOver(dest),
            (state, holder) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{drive} is {state:?} with its lineage on a {holder:?}"),
                ))
            }
        };
        Ok((previous, holder, written))
    }

    /// Sync a cut-over drive and snapshot its destination `dest` as
    /// `name`.
    async fn snapshot_under_drive(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        dest: &VolumeName,
        name: &VolumeName,
    ) -> io::Result<Volume> {
        self.disk
            .open_clone(&live_name(sandbox, role))
            .await?
            .sync()
            .await?;
        let dest = self.disk.open(dest).await?;
        self.disk.snapshot(&dest, name).await
    }

    /// Under the guest pause: sync the live volume and snapshot it. A
    /// cloned role syncs its drive and snapshots the destination, with
    /// the bitmap while the drive copies.
    pub async fn take(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<PendingCheckpoint> {
        let names = self.disk.list().await?;
        let (previous, holder, written) = self.written(&names, sandbox, role).await?;
        let tmp = transient_name(sandbox, role);
        let pending = |snapshot, shape| PendingCheckpoint {
            sandbox,
            role,
            snapshot,
            previous,
            shape,
        };
        match written {
            Written::Live => {
                self.delete_if_present(&tmp).await?;
                let live = self.live(sandbox, role).await?;
                let snapshot = checkpoint_split::take(self.disk.as_ref(), &live, &tmp).await?;
                Ok(pending(snapshot, Shape::Volume))
            }
            Written::CutOver(dest) => {
                self.discard_pair(sandbox, role).await?;
                let snapshot = self
                    .snapshot_under_drive(sandbox, role, &dest, &tmp)
                    .await?;
                let shape = match holder {
                    Holder::Checkpoint => Shape::Volume,
                    Holder::Dest => Shape::CutOver,
                };
                Ok(pending(snapshot, shape))
            }
            Written::Copying(dest) => {
                self.discard_pair(sandbox, role).await?;
                // A suspend waits for the source's reads in flight.
                self.sources(role)?.ensure_alive(sandbox, role).await?;
                let drive = live_name(sandbox, role);
                self.disk.open_clone(&drive).await?.sync().await?;
                let dest = self.disk.open(&dest).await?;
                let bitmap = self.disk.open(&bitmap_name(sandbox, role)).await?;
                let (snapshot, bitmap) = self
                    .disk
                    .snapshot_clone(
                        &drive,
                        &dest,
                        &bitmap,
                        &tmp,
                        &bitmap_snapshot_name(sandbox, role),
                    )
                    .await?;
                Ok(pending(snapshot, Shape::Hydrating { bitmap }))
            }
        }
    }

    /// The checkpoint a `take` left unpublished, found by name: a new
    /// process redrives the publish after a crash. `NotFound` when
    /// there is none. A destination snapshot without its bitmap snapshot
    /// while the drive copies is `InvalidData`: alone it reads zeros
    /// where the drive had not copied.
    pub async fn pending(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<PendingCheckpoint> {
        let names = self.disk.list().await?;
        let (previous, holder) = lineage_in(&names, sandbox, role)?;
        let tmp = transient_name(sandbox, role);
        let snapshot = self.disk.open(&tmp).await?;
        let shape = match holder {
            Holder::Checkpoint => Shape::Volume,
            Holder::Dest => match self.disk.open(&bitmap_snapshot_name(sandbox, role)).await {
                Ok(bitmap) => Shape::Hydrating { bitmap },
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    let drive = live_name(sandbox, role);
                    match self.disk.clone_state(&drive).await {
                        Ok(CloneState::CutOver) => Shape::CutOver,
                        state => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "{tmp} has no bitmap snapshot and {drive} is {state:?}; \
                                     take the checkpoint again"
                                ),
                            ))
                        }
                    }
                }
                Err(e) => return Err(e),
            },
        };
        Ok(PendingCheckpoint {
            sandbox,
            role,
            snapshot,
            previous,
            shape,
        })
    }

    /// Delete what a cloned role's earlier checkpoint left: the view and
    /// the snapshot pair.
    async fn discard_pair(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<()> {
        self.remove_clone_of(sandbox, role, &view_name(sandbox, role))
            .await?;
        self.delete_if_present(&transient_name(sandbox, role))
            .await?;
        self.delete_if_present(&bitmap_snapshot_name(sandbox, role))
            .await
    }

    /// Remove the drive or view `name` of the role; `false` when the name
    /// is not one. The role's source is recovered first: the kernel
    /// cannot remove a device that holds a read no source will finish.
    async fn remove_clone_of(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        name: &VolumeName,
    ) -> io::Result<bool> {
        if let Some(sources) = &self.sources {
            match sources.ensure_alive(sandbox, role).await {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    return Err(source_err(sandbox, role, e));
                }
                _ => {}
            }
        }
        match self.disk.remove_clone(name).await {
            Ok(()) => Ok(true),
            // `InvalidInput`: the name is a volume.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
                ) =>
            {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    fn sources(&self, role: DiskRole) -> io::Result<&dyn DiskSources> {
        self.sources.as_deref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("a cloned {role} disk needs disk sources, which this host does not have"),
            )
        })
    }

    /// A snapshot of the live volume while the guest runs, for a live
    /// move to carry before the pause. It is independent of the
    /// transient snapshot [`Self::take`] makes, so the capture can take
    /// its own beside it. A precopy left by an earlier move is replaced.
    /// A cloned role moves once a checkpoint volume newer than its
    /// destination exists, which needs the cutover; before that the move
    /// has no complete volume to carry a difference from.
    pub async fn take_precopy(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<PendingCheckpoint> {
        let names = self.disk.list().await?;
        let (previous, holder, written) = self.written(&names, sandbox, role).await?;
        let name = precopy_name(sandbox, role);
        let snapshot = match (written, holder) {
            (Written::Live, _) => {
                self.delete_if_present(&name).await?;
                let live = self.live(sandbox, role).await?;
                checkpoint_split::take(self.disk.as_ref(), &live, &name).await?
            }
            (Written::CutOver(dest), Holder::Checkpoint) => {
                self.delete_if_present(&name).await?;
                self.snapshot_under_drive(sandbox, role, &dest, &name)
                    .await?
            }
            (Written::CutOver(_) | Written::Copying(_), _) => {
                return Err(cloned_refused(
                    sandbox,
                    role,
                    "move live before its first checkpoint after the cutover",
                ))
            }
        };
        Ok(PendingCheckpoint {
            sandbox,
            role,
            snapshot,
            previous,
            shape: Shape::Volume,
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

    /// Outside the pause: upload what changed since the last checkpoint
    /// and publish the manifest at the lineage's next free version. The
    /// snapshot becomes the sandbox's checkpoint volume; a cloned role's
    /// destination takes the new version's name until the cutover.
    pub async fn publish(&self, pending: PendingCheckpoint) -> io::Result<ManifestRef> {
        let PendingCheckpoint {
            sandbox,
            role,
            snapshot,
            previous,
            shape,
        } = pending;
        let previous_name = checkpoint_name(sandbox, role, previous);
        let manifest = match &shape {
            Shape::Volume => {
                let previous_volume = self.disk.open(&previous_name).await?;
                let previous_manifest =
                    self.store.get_manifest(previous).await.map_err(store_err)?;
                checkpoint_split::upload(
                    self.disk.as_ref(),
                    &self.store,
                    &snapshot,
                    Some((&previous_volume, &previous_manifest)),
                )
                .await?
            }
            Shape::Hydrating { bitmap } => {
                self.upload_through_view(sandbox, role, &snapshot, bitmap)
                    .await?
            }
            // Every chunk is hashed; one equal to the durable previous
            // manifest's chunk is not uploaded.
            Shape::CutOver => {
                let previous_manifest =
                    self.store.get_manifest(previous).await.map_err(store_err)?;
                let every = (0..snapshot.chunk_count()).map(ChunkIndex).collect();
                checkpoint_split::upload_over(&self.store, &snapshot, every, &previous_manifest)
                    .await?
            }
        };
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
        match shape {
            Shape::Volume => {
                self.disk
                    .rename(&tmp, &checkpoint_name(sandbox, role, next))
                    .await?;
                self.disk.delete(&previous_name).await?;
            }
            Shape::CutOver => {
                self.disk
                    .rename(&tmp, &checkpoint_name(sandbox, role, next))
                    .await?;
            }
            // A crash after the rename leaves the pair, which publishes
            // again under the version after `next`, with the same bytes.
            Shape::Hydrating { bitmap } => {
                drop(bitmap);
                self.disk
                    .rename(
                        &dest_name(sandbox, role, previous),
                        &dest_name(sandbox, role, next),
                    )
                    .await?;
                self.discard_pair(sandbox, role).await?;
            }
        }
        Ok(next)
    }

    /// The source's manifest with every chunk the destination snapshot
    /// can have written since its origin replaced by what the view over
    /// the pair reads there. A chunk the destination never wrote is a
    /// chunk the drive had not copied, which the source serves.
    async fn upload_through_view(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        snapshot: &Volume,
        bitmap: &Volume,
    ) -> io::Result<Manifest> {
        let sources = self.sources(role)?;
        sources.ensure_alive(sandbox, role).await?;
        let served = sources.served(sandbox, role).await?;
        let name = view_name(sandbox, role);
        self.remove_clone_of(sandbox, role, &name).await?;
        let view = self
            .disk
            .clone_view(&name, snapshot, bitmap, &served.device)
            .await?;
        let uploaded = async {
            let origin = match served.base {
                Some(base) => match self.disk.open(&image_volumes::volume_name(base)).await {
                    Ok(origin) => Some(origin).filter(|o| o.size_bytes == snapshot.size_bytes),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                    Err(e) => return Err(e),
                },
                None => None,
            };
            let written = self.disk.written_chunks(origin.as_ref(), snapshot).await?;
            let source = self
                .store
                .get_manifest(served.manifest)
                .await
                .map_err(store_err)?;
            checkpoint_split::upload_over(&self.store, &view, written, &source).await
        }
        .await;
        drop(view);
        let removed = self.remove_clone_of(sandbox, role, &name).await;
        let manifest = uploaded?;
        removed?;
        Ok(manifest)
    }

    /// [`Self::take`] then [`Self::publish`], for a guest that is paused
    /// throughout or has no guest.
    pub async fn checkpoint(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<ManifestRef> {
        let pending = self.take(sandbox, role).await?;
        self.publish(pending).await
    }

    /// Delete a pending checkpoint's snapshots without publishing them.
    pub async fn discard(&self, pending: PendingCheckpoint) -> io::Result<()> {
        let PendingCheckpoint {
            sandbox,
            role,
            snapshot,
            shape,
            ..
        } = pending;
        let name = snapshot.name.clone();
        drop(snapshot);
        match shape {
            Shape::Hydrating { bitmap } => {
                drop(bitmap);
                self.discard_pair(sandbox, role).await
            }
            Shape::Volume | Shape::CutOver => self.disk.delete(&name).await,
        }
    }

    /// Where the role is on its way to a finished cutover; `None` when it
    /// is not a cloned role or its cutover is finished. While the drive
    /// copies, a source that died is started again first: the guest's
    /// reads of uncopied regions wait on it.
    pub async fn hydration(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<Option<Hydrating>> {
        let names = self.disk.list().await?;
        if !cloned_in(&names, sandbox, role) {
            return Ok(None);
        }
        Ok(match self.clone_progress(sandbox, role).await? {
            Some(copied) => {
                self.sources(role)?
                    .ensure_alive(sandbox, role)
                    .await
                    .map_err(|e| source_err(sandbox, role, e))?;
                Some(if copied.hydrated_regions < copied.total_regions {
                    Hydrating::Copying(copied)
                } else {
                    Hydrating::Ready
                })
            }
            None => {
                let sourced = match &self.sources {
                    Some(sources) => sources.list().await?.contains(&(sandbox, role)),
                    None => false,
                };
                (sourced || names.contains(&bitmap_name(sandbox, role))).then_some(Hydrating::Ready)
            }
        })
    }

    /// Cut a hydrated cloned role over to its destination, then delete
    /// its bitmap. The caller serializes this with the role's
    /// checkpoints, from `take` to `publish`, and then stops the source
    /// with [`Self::stop_source`] outside that lock. A crash at any step
    /// leaves a state the next call finishes: [`Self::hydration`] reports
    /// `Ready` while the bitmap or the source is left. `InvalidInput` when
    /// the role is not cloned.
    pub async fn cut_over(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<Cutover> {
        let names = self.disk.list().await?;
        if !cloned_in(&names, sandbox, role) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the {role} disk of sandbox {sandbox} is not a cloned drive"),
            ));
        }
        let progress = self.clone_progress(sandbox, role).await?;
        if let Some(copied) = progress.filter(|c| c.hydrated_regions < c.total_regions) {
            return Ok(Cutover::Waiting(Waiting::Copying(copied)));
        }
        if self.checkpoint_in_flight(&names, sandbox, role).await? {
            return Ok(Cutover::Waiting(Waiting::Checkpoint));
        }
        if progress.is_some() {
            // A suspend waits for the source's reads in flight. The caller
            // recovers a dead source before it takes the lock (through
            // `hydration`), so this check is cheap unless the source died
            // since.
            self.sources(role)?
                .ensure_alive(sandbox, role)
                .await
                .map_err(|e| source_err(sandbox, role, e))?;
            self.disk.cut_over(&live_name(sandbox, role)).await?;
        }
        self.delete_if_present(&bitmap_name(sandbox, role)).await?;
        Ok(Cutover::Done)
    }

    /// Stop the source of a role that is cut over. No drive reads it: the
    /// live drive is linear, and a checkpoint of a cut-over role makes no
    /// view. `InvalidInput` while the drive copies or a view exists. A
    /// role with no source is left as it is.
    pub async fn stop_source(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<()> {
        let Some(sources) = &self.sources else {
            return Ok(());
        };
        let names = self.disk.list().await?;
        let copies =
            cloned_in(&names, sandbox, role) && self.clone_progress(sandbox, role).await?.is_some();
        let viewed = match self.disk.clone_state(&view_name(sandbox, role)).await {
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        if copies || viewed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a drive of the {role} disk of sandbox {sandbox} still reads its source"),
            ));
        }
        sources
            .stop(sandbox, role)
            .await
            .map_err(|e| source_err(sandbox, role, e))
    }

    /// The regions the role's drive has copied; `None` once it is cut
    /// over.
    async fn clone_progress(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<Option<Copied>> {
        let drive = live_name(sandbox, role);
        match self.disk.clone_state(&drive).await? {
            CloneState::Copying {
                hydrated_regions,
                total_regions,
                ..
            } => Ok(Some(Copied {
                hydrated_regions,
                total_regions,
            })),
            CloneState::CutOver => Ok(None),
            CloneState::Incomplete => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{drive} has no table"),
            )),
        }
    }

    /// A part of a checkpoint of the role exists: the snapshot pair, or
    /// the view over it.
    async fn checkpoint_in_flight(
        &self,
        names: &[VolumeName],
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<bool> {
        if names.contains(&transient_name(sandbox, role))
            || names.contains(&bitmap_snapshot_name(sandbox, role))
        {
            return Ok(true);
        }
        match self.disk.clone_state(&view_name(sandbox, role)).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Delete the sandbox's drives, its live and transient volumes and
    /// every checkpoint volume but the newest of each role. The kept
    /// checkpoint makes a resume on this host a snapshot instead of a
    /// stage; [`Self::reap`] deletes it when the pool needs the space.
    /// A cloned role whose destination is newer than its checkpoint
    /// keeps none: the destination is not complete. Fails while a
    /// process holds a volume open.
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

    /// Delete every drive and volume of the sandbox. One a process
    /// holds open stays and is reported as failed.
    pub async fn purge(&self, sandbox: SandboxId) -> io::Result<Report> {
        let mut report = Report::default();
        let parsed: Vec<(VolumeName, Parsed)> = self
            .disk
            .list()
            .await?
            .into_iter()
            .filter_map(|name| parse(&name).map(|p| (name, p)))
            .filter(|(_, p)| p.sandbox == sandbox)
            .collect();
        let roles = self.cloned_or_sourced(&parsed, |s| s == sandbox).await?;
        let stuck = self.take_down(roles, &mut report).await;
        for (name, p) in parsed {
            if stuck.contains(&(p.sandbox, p.role)) {
                continue;
            }
            match self.disk.delete(&name).await {
                Ok(()) => report.deleted.push(name),
                Err(e) => report.failed.push((name, e)),
            }
        }
        if report.failed.is_empty() {
            self.kept.lock().unwrap().retain(|s| *s != sandbox);
        }
        Ok(report)
    }

    /// Delete what no sandbox needs: the drives and the live and
    /// transient volumes of every sandbox not in `live` and every
    /// checkpoint volume but the newest of its role. A live sandbox
    /// keeps its transient volume: it may be a checkpoint whose publish
    /// is being redriven. Run once after the live set is known, before
    /// any sandbox is provisioned.
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
    /// drives and the live and transient volumes of sandboxes outside
    /// `live`, and every checkpoint volume older than the newest
    /// checkpoint or destination of its role. A role whose drives or
    /// source could not be taken down keeps its volumes, and so does a
    /// role with two lineages, which is reported.
    async fn sweep(
        &self,
        within: impl Fn(SandboxId) -> bool,
        live: &[SandboxId],
    ) -> io::Result<Report> {
        let mut report = Report::default();
        let parsed: Vec<(VolumeName, Parsed)> = self
            .disk
            .list()
            .await?
            .into_iter()
            .filter_map(|name| parse(&name).map(|p| (name, p)))
            .filter(|(_, p)| within(p.sandbox))
            .collect();
        let holders = holders(parsed.iter().map(|(_, p)| p));
        let dead = self
            .cloned_or_sourced(&parsed, |s| within(s) && !live.contains(&s))
            .await?;
        let mut stuck = self.take_down(dead, &mut report).await;
        for (&(sandbox, role), holder) in &holders {
            if let Err(ids) = holder {
                report
                    .failed
                    .push((live_name(sandbox, role), two_lineages(sandbox, role, *ids)));
                stuck.push((sandbox, role));
            }
        }
        for (name, p) in parsed {
            let unwanted = match p.kind {
                Kind::Live
                | Kind::Transient
                | Kind::Dest(_)
                | Kind::Bitmap
                | Kind::BitmapSnapshot
                | Kind::View => !live.contains(&p.sandbox),
                Kind::Checkpoint(m) => match holders[&(p.sandbox, p.role)] {
                    Ok((newest, _)) => m.version < newest.version,
                    Err(_) => false,
                },
            };
            if !unwanted {
                if matches!(p.kind, Kind::Checkpoint(_)) && !report.kept.contains(&p.sandbox) {
                    report.kept.push(p.sandbox);
                }
                continue;
            }
            if stuck.contains(&(p.sandbox, p.role)) {
                continue;
            }
            match self.disk.delete(&name).await {
                Ok(()) => report.deleted.push(name),
                Err(e) => report.failed.push((name, e)),
            }
        }
        Ok(report)
    }

    /// The cloned roles among `parsed` and the roles with a source, of
    /// the sandboxes `select` picks.
    async fn cloned_or_sourced(
        &self,
        parsed: &[(VolumeName, Parsed)],
        select: impl Fn(SandboxId) -> bool,
    ) -> io::Result<Vec<(SandboxId, DiskRole)>> {
        let mut roles = cloned_roles(parsed.iter().map(|(_, p)| p).filter(|p| select(p.sandbox)));
        if let Some(sources) = &self.sources {
            for (sandbox, role) in sources.list().await? {
                if select(sandbox) && !roles.contains(&(sandbox, role)) {
                    roles.push((sandbox, role));
                }
            }
        }
        Ok(roles)
    }

    /// Take down each role's drives and source before its volumes go:
    /// they hold the destination, bitmap and checkpoint snapshots. A
    /// role whose step fails keeps the steps after it, and is returned
    /// so its volumes stay too.
    async fn take_down(
        &self,
        roles: Vec<(SandboxId, DiskRole)>,
        report: &mut Report,
    ) -> Vec<(SandboxId, DiskRole)> {
        let mut stuck = Vec::new();
        for (sandbox, role) in roles {
            if let Err(failed) = self.take_down_role(sandbox, role, report).await {
                report.failed.push(failed);
                stuck.push((sandbox, role));
            }
        }
        stuck
    }

    /// Recover the source, remove the view and the drive, stop the
    /// source: no drive is removed over a dead source, and no source
    /// stops under a drive.
    async fn take_down_role(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        report: &mut Report,
    ) -> Result<(), (VolumeName, io::Error)> {
        for name in [view_name(sandbox, role), live_name(sandbox, role)] {
            match self.remove_clone_of(sandbox, role, &name).await {
                Ok(true) => report.deleted.push(name),
                Ok(false) => {}
                Err(e) => return Err((name, e)),
            }
        }
        if let Some(sources) = &self.sources {
            sources
                .stop(sandbox, role)
                .await
                .map_err(|e| (live_name(sandbox, role), source_err(sandbox, role, e)))?;
        }
        Ok(())
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

    /// What the last publish of the role made durable: the manifest of
    /// its newest checkpoint volume or destination.
    pub async fn checkpoint_of(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<ManifestRef> {
        Ok(lineage_in(&self.disk.list().await?, sandbox, role)?.0)
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
    use std::path::Path;

    use engram_storage_local::LocalBlobStorage;

    use super::*;
    use crate::disk::{ChunkIndex, CloneState, FileDisk, Hydration, CLONE_HEADER_BYTES};
    use crate::disk_flows::checkpoint;
    use crate::disk_sources::{FileSources, Served};
    use crate::test_support::CountingBlob;

    const CHUNK: u64 = 4096;

    struct Rig {
        _dir: tempfile::TempDir,
        disk: Arc<dyn SessionDisk>,
        store: ChunkStore,
        volumes: SessionVolumes,
        sources: Arc<FileSources>,
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
        let sources =
            Arc::new(FileSources::new(dir.path().join("sources"), store.clone()).unwrap());
        Rig {
            volumes: SessionVolumes::new(disk.clone(), store.clone()).with_sources(sources.clone()),
            disk,
            store,
            sources,
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
        async fn clone_drive(
            &self,
            drive: &VolumeName,
            dest: &Volume,
            bitmap: &Volume,
            source: &Path,
            hydration: Hydration,
        ) -> io::Result<Volume> {
            self.0
                .clone_drive(drive, dest, bitmap, source, hydration)
                .await
        }
        async fn open_clone(&self, drive: &VolumeName) -> io::Result<Volume> {
            self.0.open_clone(drive).await
        }
        async fn clone_state(&self, drive: &VolumeName) -> io::Result<CloneState> {
            self.0.clone_state(drive).await
        }
        async fn skip_hydration(
            &self,
            drive: &VolumeName,
            ranges: &[Range<u64>],
        ) -> io::Result<()> {
            self.0.skip_hydration(drive, ranges).await
        }
        async fn enable_hydration(&self, drive: &VolumeName) -> io::Result<()> {
            self.0.enable_hydration(drive).await
        }
        async fn cut_over(&self, drive: &VolumeName) -> io::Result<()> {
            self.0.cut_over(drive).await
        }
        async fn snapshot_clone(
            &self,
            drive: &VolumeName,
            dest: &Volume,
            bitmap: &Volume,
            dest_snapshot: &VolumeName,
            bitmap_snapshot: &VolumeName,
        ) -> io::Result<(Volume, Volume)> {
            self.0
                .snapshot_clone(drive, dest, bitmap, dest_snapshot, bitmap_snapshot)
                .await
        }
        async fn clone_view(
            &self,
            view: &VolumeName,
            dest_snapshot: &Volume,
            bitmap_snapshot: &Volume,
            source: &Path,
        ) -> io::Result<Volume> {
            self.0
                .clone_view(view, dest_snapshot, bitmap_snapshot, source)
                .await
        }
        async fn remove_clone(&self, drive: &VolumeName) -> io::Result<()> {
            self.0.remove_clone(drive).await
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

    /// A disk manifest of `total` bytes in 16 MiB chunks; `hashes[i]`
    /// lists chunk `i` with that hash byte, `None` leaves it a hole.
    fn manifest_of(total: u64, hashes: &[Option<u8>]) -> Manifest {
        const MIB16: u64 = 16 << 20;
        Manifest {
            chunk_size: ChunkSize::bytes(MIB16),
            chunks: hashes
                .iter()
                .enumerate()
                .filter_map(|(i, hash)| {
                    hash.map(|h| engram_chunk_store::ChunkRef {
                        offset: i as u64 * MIB16,
                        hash: engram_chunk_store::ChunkHash::from_bytes([h; 32]),
                    })
                })
                .collect(),
            ..Manifest::empty(ManifestKind::Disk, total)
        }
    }

    const MIB: u64 = 1 << 20;

    #[test]
    fn a_new_destination_skips_the_chunks_the_manifest_does_not_list() {
        let wanted = manifest_of(64 * MIB, &[None, Some(1), None, None]);
        assert_eq!(
            hydration_skips(&wanted, None).unwrap(),
            vec![0..16 * MIB, 32 * MIB..64 * MIB]
        );
        let full = manifest_of(32 * MIB, &[Some(1), Some(2)]);
        assert_eq!(hydration_skips(&full, None).unwrap(), vec![]);
        let empty = manifest_of(32 * MIB, &[]);
        assert_eq!(hydration_skips(&empty, None).unwrap(), vec![0..32 * MIB]);
    }

    #[test]
    fn a_base_snapshot_skips_the_chunks_equal_to_the_base() {
        // Equal, different, a hole in both, listed only in the base,
        // listed only in the wanted manifest, equal.
        let base = manifest_of(96 * MIB, &[Some(1), Some(2), None, Some(4), None, Some(6)]);
        let wanted = manifest_of(96 * MIB, &[Some(1), Some(9), None, None, Some(5), Some(6)]);
        assert_eq!(
            hydration_skips(&wanted, Some(&base)).unwrap(),
            vec![0..16 * MIB, 32 * MIB..48 * MIB, 80 * MIB..96 * MIB]
        );
        assert_eq!(
            hydration_skips(&wanted, Some(&wanted)).unwrap(),
            vec![0..96 * MIB]
        );
        let other_size = manifest_of(80 * MIB, &[Some(1)]);
        assert!(hydration_skips(&wanted, Some(&other_size)).is_err());
    }

    #[test]
    fn a_short_last_chunk_is_skipped_up_to_its_last_whole_region() {
        let region = CLONE_REGION_BYTES;
        let total = 32 * MIB + 3 * region + 512;
        let wanted = manifest_of(total, &[Some(1), None, None]);
        assert_eq!(
            hydration_skips(&wanted, None).unwrap(),
            vec![16 * MIB..32 * MIB + 3 * region]
        );
        // A chunk smaller than a region: only regions whose every chunk
        // qualifies are skipped.
        let small = Manifest {
            chunk_size: ChunkSize::bytes(region / 4),
            chunks: [1u64, 9]
                .iter()
                .map(|i| engram_chunk_store::ChunkRef {
                    offset: i * region / 4,
                    hash: engram_chunk_store::ChunkHash::from_bytes([1; 32]),
                })
                .collect(),
            ..Manifest::empty(ManifestKind::Disk, 4 * region)
        };
        assert_eq!(
            hydration_skips(&small, None).unwrap(),
            vec![region..2 * region, 3 * region..4 * region]
        );
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
        for (name, kind) in [
            (transient_name(sandbox, DiskRole::Root), Kind::Transient),
            (precopy_name(sandbox, DiskRole::Root), Kind::Transient),
            (
                dest_name(sandbox, DiskRole::Root, manifest),
                Kind::Dest(manifest),
            ),
            (bitmap_name(sandbox, DiskRole::Root), Kind::Bitmap),
            (
                bitmap_snapshot_name(sandbox, DiskRole::Root),
                Kind::BitmapSnapshot,
            ),
            (view_name(sandbox, DiskRole::Root), Kind::View),
        ] {
            assert_eq!(
                parse(&name),
                Some(Parsed {
                    sandbox,
                    role: DiskRole::Root,
                    kind
                }),
                "{name}"
            );
        }
        let s = sandbox.as_uuid().simple();
        let m = manifest.manifest_id.simple();
        for (name, want) in [
            (live_name(sandbox, DiskRole::Root), format!("sb-{s}-root")),
            (
                checkpoint_name(sandbox, DiskRole::Root, manifest),
                format!("sb-{s}-root-ckpt-{m}-v1"),
            ),
            (
                dest_name(sandbox, DiskRole::Root, manifest),
                format!("sb-{s}-root-dest-{m}-v1"),
            ),
            (
                bitmap_name(sandbox, DiskRole::Root),
                format!("sb-{s}-root-bitmap"),
            ),
            (
                transient_name(sandbox, DiskRole::Root),
                format!("tmp-sb-{s}-root"),
            ),
            (
                precopy_name(sandbox, DiskRole::Root),
                format!("tmp-sb-{s}-root-pre"),
            ),
            (
                bitmap_snapshot_name(sandbox, DiskRole::Root),
                format!("tmp-sb-{s}-root-bm"),
            ),
            (
                view_name(sandbox, DiskRole::Root),
                format!("tmp-sb-{s}-root-view"),
            ),
        ] {
            assert_eq!(name.as_str(), want);
        }
        let longest = dest_name(
            sandbox,
            DiskRole::Root,
            ManifestRef {
                version: u64::MAX,
                ..manifest
            },
        );
        assert_eq!(longest.as_str().len(), 100);
        for bad in [
            format!("sb-{s}-root-pre"),
            format!("sb-{s}-root-bm"),
            format!("tmp-sb-{s}-root-bitmap"),
            format!("tmp-sb-{s}-root-dest-{m}-v1"),
            format!("sb-{s}-root-dest-{m}"),
        ] {
            assert_eq!(parse(&VolumeName::new(bad.clone()).unwrap()), None, "{bad}");
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

        let restarted = SessionVolumes::new(r.disk.clone(), r.store.clone());
        let report = restarted.reconcile(&[]).await.unwrap();
        assert_eq!(report.kept, vec![dead]);
        assert_eq!(checkpoints_of(&r, dead, DiskRole::Root).await.len(), 1);

        let reaped = restarted.reap(POOL_TOTAL).await.unwrap();
        assert_eq!(reaped.deleted.len(), 1);
        assert!(checkpoints_of(&r, dead, DiskRole::Root).await.is_empty());
    }

    /// Make `role` of `sandbox` cloned as a lazy restore does: a
    /// destination named after `lineage`, a bitmap and the drive over
    /// them. Returns the drive.
    async fn cloned_role(
        r: &Rig,
        sandbox: SandboxId,
        role: DiskRole,
        lineage: ManifestRef,
    ) -> Volume {
        let dest = r
            .disk
            .create(&dest_name(sandbox, role, lineage), 2 * CHUNK)
            .await
            .unwrap();
        let bitmap = r
            .disk
            .create(&bitmap_name(sandbox, role), CLONE_HEADER_BYTES)
            .await
            .unwrap();
        let source = r._dir.path().join(format!("source-{sandbox}-{role}"));
        std::fs::write(&source, vec![7u8; 2 * CHUNK as usize]).unwrap();
        let hydration = Hydration {
            enabled: true,
            threshold_regions: 1,
            batch_regions: 1,
        };
        r.disk
            .clone_drive(
                &live_name(sandbox, role),
                &dest,
                &bitmap,
                &source,
                hydration,
            )
            .await
            .unwrap()
    }

    /// A cloned role's checkpoint in flight: the snapshot pair and the
    /// view over it.
    async fn cloned_checkpoint(r: &Rig, sandbox: SandboxId, role: DiskRole, lineage: ManifestRef) {
        let dest = r
            .disk
            .open(&dest_name(sandbox, role, lineage))
            .await
            .unwrap();
        let bitmap = r.disk.open(&bitmap_name(sandbox, role)).await.unwrap();
        let (dest_snap, bitmap_snap) = r
            .disk
            .snapshot_clone(
                &live_name(sandbox, role),
                &dest,
                &bitmap,
                &transient_name(sandbox, role),
                &bitmap_snapshot_name(sandbox, role),
            )
            .await
            .unwrap();
        r.disk
            .clone_view(
                &view_name(sandbox, role),
                &dest_snap,
                &bitmap_snap,
                Path::new("/dev/null"),
            )
            .await
            .unwrap();
    }

    fn version(lineage: ManifestRef, version: u64) -> ManifestRef {
        ManifestRef { version, ..lineage }
    }

    #[tokio::test]
    async fn a_cloned_role_is_a_role_whose_drive_the_hypervisor_opens() {
        let r = rig();
        let sandbox = SandboxId::new();
        let lineage = ManifestRef::new();
        r.disk
            .create(
                &checkpoint_name(sandbox, DiskRole::Root, lineage),
                2 * CHUNK,
            )
            .await
            .unwrap();
        let drive = cloned_role(&r, sandbox, DiskRole::Root, version(lineage, 2)).await;

        assert_eq!(
            r.volumes.roles(sandbox).await.unwrap(),
            vec![DiskRole::Root]
        );
        assert_eq!(
            r.volumes.path(sandbox, DiskRole::Root).await.unwrap(),
            drive.path
        );
        assert_eq!(
            r.volumes
                .checkpoint_of(sandbox, DiskRole::Root)
                .await
                .unwrap(),
            version(lineage, 2)
        );
        assert_eq!(
            r.volumes
                .live(sandbox, DiskRole::Root)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[tokio::test]
    async fn a_cloned_role_refuses_a_live_move_and_finds_its_pending_pair() {
        let r = rig();
        let sandbox = SandboxId::new();
        let lineage = ManifestRef::new();
        cloned_role(&r, sandbox, DiskRole::Root, lineage).await;
        assert_eq!(
            r.volumes
                .pending(sandbox, DiskRole::Root)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        cloned_checkpoint(&r, sandbox, DiskRole::Root, lineage).await;
        let before = names(&r).await;

        let refused = r
            .volumes
            .take_precopy(sandbox, DiskRole::Root)
            .await
            .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::Unsupported, "{refused}");
        assert_eq!(names(&r).await, before);
        let pending = r.volumes.pending(sandbox, DiskRole::Root).await.unwrap();
        assert!(matches!(pending.shape, Shape::Hydrating { .. }));
        assert_eq!(pending.previous(), lineage);
    }

    #[tokio::test]
    async fn release_of_a_cloned_role_removes_its_drives_and_keeps_no_checkpoint() {
        let r = rig();
        let sandbox = SandboxId::new();
        let lineage = ManifestRef::new();
        r.disk
            .create(
                &checkpoint_name(sandbox, DiskRole::Root, lineage),
                2 * CHUNK,
            )
            .await
            .unwrap();
        cloned_role(&r, sandbox, DiskRole::Root, version(lineage, 2)).await;
        cloned_checkpoint(&r, sandbox, DiskRole::Root, version(lineage, 2)).await;

        r.volumes.release(sandbox).await.unwrap();

        assert!(names(&r).await.is_empty(), "{:?}", names(&r).await);
        for drive in [
            live_name(sandbox, DiskRole::Root),
            view_name(sandbox, DiskRole::Root),
        ] {
            assert_eq!(
                r.disk.open_clone(&drive).await.unwrap_err().kind(),
                io::ErrorKind::NotFound
            );
        }
        assert!(r.volumes.released().is_empty());
    }

    /// After the cutover a checkpoint volume newer than the destination
    /// is complete, so a release keeps it.
    #[tokio::test]
    async fn release_keeps_a_checkpoint_newer_than_the_destination() {
        let r = rig();
        let sandbox = SandboxId::new();
        let lineage = ManifestRef::new();
        cloned_role(&r, sandbox, DiskRole::Root, lineage).await;
        let newer = checkpoint_name(sandbox, DiskRole::Root, version(lineage, 2));
        r.disk.create(&newer, 2 * CHUNK).await.unwrap();

        r.volumes.release(sandbox).await.unwrap();

        assert_eq!(names(&r).await, vec![newer.as_str().to_string()]);
        assert_eq!(r.volumes.released(), vec![sandbox]);
    }

    #[tokio::test]
    async fn reconcile_keeps_a_live_cloned_role_and_purge_removes_it() {
        let r = rig();
        let (live, dead) = (SandboxId::new(), SandboxId::new());
        for sandbox in [live, dead] {
            let lineage = ManifestRef::new();
            cloned_role(&r, sandbox, DiskRole::Root, lineage).await;
            cloned_checkpoint(&r, sandbox, DiskRole::Root, lineage).await;
        }
        let mut kept: Vec<String> = names(&r).await;
        kept.retain(|n| parse(&VolumeName::new(n.clone()).unwrap()).unwrap().sandbox == live);

        let report = r.volumes.reconcile(&[live]).await.unwrap();

        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(names(&r).await, kept);
        r.disk
            .open_clone(&live_name(live, DiskRole::Root))
            .await
            .unwrap();
        r.disk
            .open_clone(&view_name(live, DiskRole::Root))
            .await
            .unwrap();
        assert_eq!(
            r.disk
                .open_clone(&live_name(dead, DiskRole::Root))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );

        let report = r.volumes.purge(live).await.unwrap();

        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(names(&r).await.is_empty(), "{:?}", names(&r).await);
        assert_eq!(
            r.disk
                .open_clone(&live_name(live, DiskRole::Root))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    /// A manifest of the sandbox's own lineage with chunk `i` holding
    /// `pattern[i]`, published over a fork of `image`.
    async fn published(r: &Rig, image: ManifestRef, pattern: &[u8]) -> ManifestRef {
        let sandbox = SandboxId::new();
        let p = r
            .volumes
            .provision(sandbox, fork(image), None)
            .await
            .unwrap();
        for (i, byte) in pattern.iter().enumerate() {
            write_byte(&p.root, i as u64, *byte).await;
        }
        let manifest = r.volumes.checkpoint(sandbox, DiskRole::Root).await.unwrap();
        r.volumes.purge(sandbox).await.unwrap();
        manifest
    }

    fn lazy_fork(image: ManifestRef) -> Source {
        Source::LazyFork {
            manifest: image,
            base: None,
        }
    }

    #[tokio::test]
    async fn a_lazy_provision_reads_the_manifest_through_a_drive() {
        let r = rig();
        let image = staged_image(&r, &[1, 2, 0]).await;
        let forked = SandboxId::new();

        let p = r
            .volumes
            .provision(forked, lazy_fork(image), None)
            .await
            .unwrap();

        assert_eq!(bytes_at(&p.root).await, vec![1, 2, 0]);
        assert_ne!(p.root_manifest, image);
        assert_eq!(
            r.store.get_manifest(p.root_manifest).await.unwrap(),
            r.store.get_manifest(image).await.unwrap()
        );
        assert_eq!(
            r.volumes.path(forked, DiskRole::Root).await.unwrap(),
            p.root
        );
        assert_eq!(
            r.volumes
                .checkpoint_of(forked, DiskRole::Root)
                .await
                .unwrap(),
            p.root_manifest
        );
        assert_eq!(
            r.sources.list().await.unwrap(),
            vec![(forked, DiskRole::Root)]
        );

        let manifest = published(&r, image, &[1, 9, 0]).await;
        let continued = SandboxId::new();
        let p = r
            .volumes
            .provision(
                continued,
                Source::LazyContinue {
                    manifest,
                    base: Some(image),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(bytes_at(&p.root).await, vec![1, 9, 0]);
        assert_eq!(p.root_manifest, manifest);
        assert!(names(&r).await.contains(
            &dest_name(continued, DiskRole::Root, manifest)
                .as_str()
                .to_string()
        ));
    }

    #[tokio::test]
    async fn a_failed_lazy_provision_leaves_nothing_of_the_sandbox() {
        let r = rig();
        let image = staged_image(&r, &[1]).await;
        let only_image = vec![image_volumes::volume_name(image).as_str().to_string()];
        let sandbox = SandboxId::new();
        // The swap drive's name is taken, so the swap fails after its
        // source started.
        r.disk
            .create(&live_name(sandbox, DiskRole::Swap), CHUNK)
            .await
            .unwrap();

        let err = r
            .volumes
            .provision(sandbox, lazy_fork(image), Some(lazy_fork(image)))
            .await
            .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(names(&r).await, only_image);
        assert!(r.sources.list().await.unwrap().is_empty());
        assert_eq!(
            r.disk
                .open_clone(&live_name(sandbox, DiskRole::Root))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );

        let without = SessionVolumes::new(r.disk.clone(), r.store.clone());
        let err = without
            .provision(sandbox, lazy_fork(image), None)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported, "{err}");
        assert_eq!(names(&r).await, only_image);
    }

    /// [`FileSources`] that logs each recovery and stop, and whether the
    /// role's drive still existed at the stop.
    struct Watched {
        inner: FileSources,
        disk: Arc<dyn SessionDisk>,
        log: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl DiskSources for Watched {
        fn can_start(&self) -> bool {
            true
        }
        async fn start(
            &self,
            sandbox: SandboxId,
            role: DiskRole,
            manifest: ManifestRef,
            base: Option<(&Volume, ManifestRef)>,
        ) -> io::Result<PathBuf> {
            self.inner.start(sandbox, role, manifest, base).await
        }
        async fn ensure_alive(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<()> {
            self.log.lock().unwrap().push(format!("alive {role}"));
            self.inner.ensure_alive(sandbox, role).await
        }
        async fn served(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<Served> {
            self.inner.served(sandbox, role).await
        }
        async fn stop(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<()> {
            let drive = self
                .disk
                .open_clone(&live_name(sandbox, role))
                .await
                .is_ok();
            self.log
                .lock()
                .unwrap()
                .push(format!("stop {role} drive={drive}"));
            self.inner.stop(sandbox, role).await
        }
        async fn list(&self) -> io::Result<Vec<(SandboxId, DiskRole)>> {
            self.inner.list().await
        }
    }

    #[tokio::test]
    async fn release_recovers_the_source_removes_the_drive_then_stops_the_source() {
        let r = rig();
        let image = staged_image(&r, &[1]).await;
        let watched = Arc::new(Watched {
            inner: FileSources::new(r._dir.path().join("watched"), r.store.clone()).unwrap(),
            disk: r.disk.clone(),
            log: Mutex::new(Vec::new()),
        });
        let volumes =
            SessionVolumes::new(r.disk.clone(), r.store.clone()).with_sources(watched.clone());
        let sandbox = SandboxId::new();
        volumes
            .provision(sandbox, lazy_fork(image), None)
            .await
            .unwrap();

        volumes.release(sandbox).await.unwrap();

        assert_eq!(
            *watched.log.lock().unwrap(),
            vec![
                "alive root".to_string(),
                "alive root".to_string(),
                "stop root drive=false".to_string()
            ]
        );
        assert!(watched.inner.list().await.unwrap().is_empty());
        assert_eq!(
            names(&r).await,
            vec![image_volumes::volume_name(image).as_str().to_string()]
        );
        assert!(volumes.released().is_empty());
    }

    /// The bytes of `manifest`, staged into a scratch volume.
    async fn staged_bytes(r: &Rig, manifest: ManifestRef) -> Vec<u8> {
        let contents = r.store.get_manifest(manifest).await.unwrap();
        let name = VolumeName::new(format!("check-{}", manifest.version)).unwrap();
        let volume = stage(r.disk.as_ref(), &r.store, &name, &contents, None)
            .await
            .unwrap();
        let bytes = std::fs::read(&volume.path).unwrap();
        drop(volume);
        r.disk.delete(&name).await.unwrap();
        bytes
    }

    fn next(manifest: ManifestRef) -> ManifestRef {
        version(manifest, manifest.version + 1)
    }

    #[tokio::test]
    async fn a_checkpoint_while_hydrating_publishes_what_the_drive_reads() {
        let r = rig();
        let image = staged_image(&r, &[1, 2, 0, 3]).await;
        let sandbox = SandboxId::new();
        let p = r
            .volumes
            .provision(sandbox, lazy_fork(image), None)
            .await
            .unwrap();
        write_half(&p.root, 0, 1, 7).await;
        write_byte(&p.root, 2, 9).await;

        let published = r.volumes.checkpoint(sandbox, DiskRole::Root).await.unwrap();

        assert_eq!(published, next(p.root_manifest));
        assert_eq!(
            staged_bytes(&r, published).await,
            std::fs::read(&p.root).unwrap()
        );
        let names = names(&r).await;
        assert!(names.contains(&dest_name(sandbox, DiskRole::Root, published).to_string()));
        for gone in [
            dest_name(sandbox, DiskRole::Root, p.root_manifest),
            transient_name(sandbox, DiskRole::Root),
            bitmap_snapshot_name(sandbox, DiskRole::Root),
        ] {
            assert!(!names.contains(&gone.to_string()), "{gone}");
        }
        let view = r.disk.open_clone(&view_name(sandbox, DiskRole::Root)).await;
        assert_eq!(view.unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    /// A process death after the take, and one inside the publish that
    /// left the view, are redriven from the names.
    #[tokio::test]
    async fn a_hydrating_checkpoint_cut_after_the_take_publishes_from_pending() {
        let r = rig();
        let image = staged_image(&r, &[1, 2]).await;
        let sandbox = SandboxId::new();
        let p = r
            .volumes
            .provision(sandbox, lazy_fork(image), None)
            .await
            .unwrap();
        write_byte(&p.root, 1, 8).await;
        drop(r.volumes.take(sandbox, DiskRole::Root).await.unwrap());
        write_byte(&p.root, 0, 4).await;
        let tmp = r
            .disk
            .open(&transient_name(sandbox, DiskRole::Root))
            .await
            .unwrap();
        let bm = r
            .disk
            .open(&bitmap_snapshot_name(sandbox, DiskRole::Root))
            .await
            .unwrap();
        r.disk
            .clone_view(
                &view_name(sandbox, DiskRole::Root),
                &tmp,
                &bm,
                Path::new("/dev/null"),
            )
            .await
            .unwrap();
        drop((tmp, bm));

        let pending = r.volumes.pending(sandbox, DiskRole::Root).await.unwrap();
        let published = r.volumes.publish(pending).await.unwrap();

        assert_eq!(published, next(p.root_manifest));
        assert_eq!(
            staged_bytes(&r, published).await[..CHUNK as usize],
            vec![1; CHUNK as usize]
        );
        assert_eq!(
            staged_bytes(&r, published).await[CHUNK as usize..],
            vec![8; CHUNK as usize]
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
    async fn after_the_cutover_a_checkpoint_hashes_the_disk_and_the_next_one_diffs() {
        let r = rig();
        let image = staged_image(&r, &[1, 2, 0, 3]).await;
        let sandbox = SandboxId::new();
        let p = r
            .volumes
            .provision(sandbox, lazy_fork(image), None)
            .await
            .unwrap();
        r.disk
            .cut_over(&live_name(sandbox, DiskRole::Root))
            .await
            .unwrap();
        write_byte(&p.root, 1, 5).await;

        let first = r.volumes.checkpoint(sandbox, DiskRole::Root).await.unwrap();

        assert_eq!(first, next(p.root_manifest));
        assert_eq!(
            staged_bytes(&r, first).await,
            std::fs::read(&p.root).unwrap()
        );
        assert_eq!(
            r.volumes
                .checkpoint_of(sandbox, DiskRole::Root)
                .await
                .unwrap(),
            first
        );
        let names_now = names(&r).await;
        assert!(names_now.contains(&checkpoint_name(sandbox, DiskRole::Root, first).to_string()));
        assert!(
            names_now.contains(&dest_name(sandbox, DiskRole::Root, p.root_manifest).to_string())
        );

        write_byte(&p.root, 3, 6).await;
        let pending = r.volumes.take(sandbox, DiskRole::Root).await.unwrap();
        assert!(
            matches!(pending.shape, Shape::Volume),
            "{:?}",
            pending.shape
        );
        assert_eq!(
            r.volumes.changed_blocks(&pending, CHUNK).await.unwrap(),
            vec![3]
        );
        let second = r.volumes.publish(pending).await.unwrap();

        assert_eq!(second, next(first));
        assert_eq!(
            staged_bytes(&r, second).await,
            std::fs::read(&p.root).unwrap()
        );
        assert!(!names(&r)
            .await
            .contains(&checkpoint_name(sandbox, DiskRole::Root, first).to_string()));

        r.volumes.release(sandbox).await.unwrap();

        assert_eq!(
            names(&r).await,
            vec![
                image_volumes::volume_name(image).to_string(),
                checkpoint_name(sandbox, DiskRole::Root, second).to_string(),
            ]
        );
        assert_eq!(r.volumes.released(), vec![sandbox]);
    }

    /// A destination snapshot left without its bitmap snapshot (a
    /// cleanup cut between the two) reads zeros where the drive had not
    /// copied, so it never publishes; the next take replaces it.
    #[tokio::test]
    async fn a_lone_destination_snapshot_while_copying_is_refused() {
        let r = rig();
        let image = staged_image(&r, &[1, 2]).await;
        let sandbox = SandboxId::new();
        r.volumes
            .provision(sandbox, lazy_fork(image), None)
            .await
            .unwrap();
        drop(r.volumes.take(sandbox, DiskRole::Root).await.unwrap());
        r.disk
            .delete(&bitmap_snapshot_name(sandbox, DiskRole::Root))
            .await
            .unwrap();

        let err = r
            .volumes
            .pending(sandbox, DiskRole::Root)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");

        let pending = r.volumes.take(sandbox, DiskRole::Root).await.unwrap();
        assert!(matches!(pending.shape, Shape::Hydrating { .. }));
    }

    /// After the cutover and a checkpoint volume newer than the
    /// destination, the role opens and moves as a plain role does; its
    /// live name stays a drive.
    #[tokio::test]
    async fn a_cut_over_role_moves_live_after_its_first_checkpoint() {
        let r = rig();
        let image = staged_image(&r, &[1, 2, 0]).await;
        let sandbox = SandboxId::new();
        let p = r
            .volumes
            .provision(sandbox, lazy_fork(image), None)
            .await
            .unwrap();
        let refused = r.volumes.take_precopy(sandbox, DiskRole::Root).await;
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::Unsupported);
        let refused = r.volumes.live(sandbox, DiskRole::Root).await;
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::Unsupported);
        r.disk
            .cut_over(&live_name(sandbox, DiskRole::Root))
            .await
            .unwrap();
        let refused = r.volumes.take_precopy(sandbox, DiskRole::Root).await;
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::Unsupported);

        r.volumes.checkpoint(sandbox, DiskRole::Root).await.unwrap();
        write_byte(&p.root, 2, 7).await;

        assert_eq!(
            r.volumes.roles(sandbox).await.unwrap(),
            vec![DiskRole::Root]
        );
        assert_eq!(
            r.volumes.live(sandbox, DiskRole::Root).await.unwrap().path,
            p.root
        );
        assert_eq!(
            r.volumes.path(sandbox, DiskRole::Root).await.unwrap(),
            p.root
        );
        let precopy = r
            .volumes
            .take_precopy(sandbox, DiskRole::Root)
            .await
            .unwrap();
        assert_eq!(
            r.volumes.changed_blocks(&precopy, CHUNK).await.unwrap(),
            vec![2]
        );
        r.volumes.discard(precopy).await.unwrap();
    }

    async fn sourced(r: &Rig, sandbox: SandboxId) -> bool {
        r.sources
            .list()
            .await
            .unwrap()
            .contains(&(sandbox, DiskRole::Root))
    }

    /// A checkpoint's publish reads through a view over the source, so a
    /// cutover waits while its pair or view exists, and goes ahead once it
    /// is published.
    #[tokio::test]
    async fn a_cutover_waits_for_a_checkpoint_in_flight() {
        let r = rig();
        let image = staged_image(&r, &[1, 2, 0]).await;
        let sandbox = SandboxId::new();
        let p = r
            .volumes
            .provision(sandbox, lazy_fork(image), None)
            .await
            .unwrap();
        write_byte(&p.root, 2, 5).await;
        let drive = live_name(sandbox, DiskRole::Root);
        let pending = r.volumes.take(sandbox, DiskRole::Root).await.unwrap();
        let tmp = r.disk.open(&transient_name(sandbox, DiskRole::Root)).await;
        let bm = r
            .disk
            .open(&bitmap_snapshot_name(sandbox, DiskRole::Root))
            .await;
        r.disk
            .clone_view(
                &view_name(sandbox, DiskRole::Root),
                &tmp.unwrap(),
                &bm.unwrap(),
                Path::new("/dev/null"),
            )
            .await
            .unwrap();
        let before = names(&r).await;

        for _ in 0..2 {
            assert_eq!(
                r.volumes.hydration(sandbox, DiskRole::Root).await.unwrap(),
                Some(Hydrating::Ready)
            );
            assert_eq!(
                r.volumes.cut_over(sandbox, DiskRole::Root).await.unwrap(),
                Cutover::Waiting(Waiting::Checkpoint)
            );
            assert_eq!(names(&r).await, before);
            assert!(sourced(&r, sandbox).await);
            assert!(matches!(
                r.disk.clone_state(&drive).await.unwrap(),
                CloneState::Copying { .. }
            ));
            // A crash after the take leaves the pair alone.
            r.volumes
                .remove_clone_of(sandbox, DiskRole::Root, &view_name(sandbox, DiskRole::Root))
                .await
                .unwrap();
        }
        let published = r.volumes.publish(pending).await.unwrap();
        let err = r
            .volumes
            .stop_source(sandbox, DiskRole::Root)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "the drive copies");

        assert_eq!(
            r.volumes.cut_over(sandbox, DiskRole::Root).await.unwrap(),
            Cutover::Done
        );
        assert_eq!(
            r.disk.clone_state(&drive).await.unwrap(),
            CloneState::CutOver
        );
        // The source stops outside the cutover, which leaves it.
        assert!(sourced(&r, sandbox).await);
        assert_eq!(
            r.volumes.hydration(sandbox, DiskRole::Root).await.unwrap(),
            Some(Hydrating::Ready)
        );
        r.volumes
            .stop_source(sandbox, DiskRole::Root)
            .await
            .unwrap();
        assert!(!sourced(&r, sandbox).await);
        assert!(!names(&r)
            .await
            .contains(&bitmap_name(sandbox, DiskRole::Root).to_string()));
        assert_eq!(
            r.volumes.hydration(sandbox, DiskRole::Root).await.unwrap(),
            None
        );
        assert_eq!(
            r.volumes.cut_over(sandbox, DiskRole::Root).await.unwrap(),
            Cutover::Done
        );
        assert_eq!(bytes_at(&p.root).await, vec![1, 2, 5]);
        assert_eq!(
            staged_bytes(&r, published).await,
            std::fs::read(&p.root).unwrap()
        );

        let plain = SandboxId::new();
        r.volumes.provision(plain, fork(image), None).await.unwrap();
        assert_eq!(
            r.volumes.hydration(plain, DiskRole::Root).await.unwrap(),
            None
        );
        let err = r.volumes.cut_over(plain, DiskRole::Root).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
    }

    /// A crash after the drive is cut over leaves the bitmap and the
    /// source, and one after the bitmap delete leaves the source. The
    /// next call finishes either; until then the role checkpoints and
    /// releases as a cut-over role.
    #[tokio::test]
    async fn a_cutover_cut_short_is_finished_by_the_next_call() {
        let r = rig();
        let image = staged_image(&r, &[1, 2]).await;
        let (whole, unfinished) = (SandboxId::new(), SandboxId::new());
        for sandbox in [whole, unfinished] {
            r.volumes
                .provision(sandbox, lazy_fork(image), None)
                .await
                .unwrap();
            r.disk
                .cut_over(&live_name(sandbox, DiskRole::Root))
                .await
                .unwrap();
        }
        r.disk
            .delete(&bitmap_name(whole, DiskRole::Root))
            .await
            .unwrap();

        for sandbox in [whole, unfinished] {
            assert_eq!(
                r.volumes.hydration(sandbox, DiskRole::Root).await.unwrap(),
                Some(Hydrating::Ready)
            );
        }
        assert_eq!(
            r.volumes.cut_over(whole, DiskRole::Root).await.unwrap(),
            Cutover::Done
        );
        r.volumes.stop_source(whole, DiskRole::Root).await.unwrap();
        assert!(!sourced(&r, whole).await);
        assert_eq!(
            r.volumes.hydration(whole, DiskRole::Root).await.unwrap(),
            None
        );

        let first = r
            .volumes
            .checkpoint(unfinished, DiskRole::Root)
            .await
            .unwrap();
        assert!(names(&r)
            .await
            .contains(&checkpoint_name(unfinished, DiskRole::Root, first).to_string()));
        assert!(sourced(&r, unfinished).await);
        for sandbox in [whole, unfinished] {
            r.volumes.release(sandbox).await.unwrap();
        }
        assert!(r.sources.list().await.unwrap().is_empty());
        assert_eq!(
            names(&r).await,
            vec![
                image_volumes::volume_name(image).to_string(),
                checkpoint_name(unfinished, DiskRole::Root, first).to_string(),
            ]
        );
    }

    /// After a cutover through the volumes the first checkpoint hashes
    /// the disk into a checkpoint volume, and a release keeps it.
    #[tokio::test]
    async fn after_a_cutover_the_first_checkpoint_is_a_checkpoint_volume() {
        let r = rig();
        let image = staged_image(&r, &[1, 2, 0, 3]).await;
        let sandbox = SandboxId::new();
        let p = r
            .volumes
            .provision(sandbox, lazy_fork(image), None)
            .await
            .unwrap();
        write_byte(&p.root, 1, 5).await;
        assert_eq!(
            r.volumes.cut_over(sandbox, DiskRole::Root).await.unwrap(),
            Cutover::Done
        );

        let first = r.volumes.checkpoint(sandbox, DiskRole::Root).await.unwrap();

        assert_eq!(first, next(p.root_manifest));
        assert_eq!(
            staged_bytes(&r, first).await,
            std::fs::read(&p.root).unwrap()
        );
        r.volumes.release(sandbox).await.unwrap();
        assert_eq!(
            names(&r).await,
            vec![
                image_volumes::volume_name(image).to_string(),
                checkpoint_name(sandbox, DiskRole::Root, first).to_string(),
            ]
        );
    }

    /// The plan of an eager provision names the volume the provision
    /// would start from, so it lists exactly the chunks it would write.
    #[tokio::test]
    async fn an_eager_plan_starts_where_the_provision_would() {
        let r = rig();
        let image = staged_image(&r, &[1, 2, 0]).await;
        let sandbox = SandboxId::new();
        let plan = |source| r.volumes.eager_plan(sandbox, DiskRole::Root, source);
        let image_manifest = r.store.get_manifest(image).await.unwrap();

        assert!(plan(fork(image)).await.unwrap().is_none());
        let empty = plan(Source::Empty { size_bytes: CHUNK }).await;
        assert!(empty.unwrap().is_none());
        let manifest = published(&r, image, &[1, 9, 0]).await;
        let wanted = r.store.get_manifest(manifest).await.unwrap();
        for (source, base) in [
            (
                Source::Fork {
                    manifest,
                    base: None,
                },
                None,
            ),
            (
                Source::LazyContinue {
                    manifest,
                    base: Some(image),
                },
                Some(&image_manifest),
            ),
        ] {
            let p = plan(source).await.unwrap().unwrap();
            assert_eq!(p.wanted, wanted);
            assert_eq!(p.base.as_ref(), base);
        }

        // A released sandbox keeps its checkpoint of the lineage: a resume
        // at that version adopts it, one at a newer version stages onto it.
        let continued = |manifest| Source::Continue {
            manifest,
            base: Some(image),
        };
        r.volumes
            .provision(sandbox, continued(manifest), None)
            .await
            .unwrap();
        r.volumes.release(sandbox).await.unwrap();
        let resumed = SandboxId::new();
        let plan = r
            .volumes
            .eager_plan(resumed, DiskRole::Root, continued(manifest));
        assert!(plan.await.unwrap().is_none());
        let newer = next(manifest);
        r.store.put_manifest(newer, &image_manifest).await.unwrap();
        let plan = r
            .volumes
            .eager_plan(resumed, DiskRole::Root, continued(newer));
        let plan = plan.await.unwrap().unwrap();
        assert_eq!(plan.wanted, image_manifest);
        assert_eq!(plan.base, Some(wanted));
    }

    /// [`FileSources`] whose recovery always fails.
    struct DeadSources(FileSources);

    #[async_trait::async_trait]
    impl DiskSources for DeadSources {
        fn can_start(&self) -> bool {
            true
        }
        async fn start(
            &self,
            sandbox: SandboxId,
            role: DiskRole,
            manifest: ManifestRef,
            base: Option<(&Volume, ManifestRef)>,
        ) -> io::Result<PathBuf> {
            self.0.start(sandbox, role, manifest, base).await
        }
        async fn ensure_alive(&self, _: SandboxId, _: DiskRole) -> io::Result<()> {
            Err(io::Error::other("the source does not start"))
        }
        async fn served(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<Served> {
            self.0.served(sandbox, role).await
        }
        async fn stop(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<()> {
            self.0.stop(sandbox, role).await
        }
        async fn list(&self) -> io::Result<Vec<(SandboxId, DiskRole)>> {
            self.0.list().await
        }
    }

    /// A role whose drive cannot be removed keeps every volume the drive
    /// holds, for the next sweep.
    #[tokio::test]
    async fn a_failed_take_down_keeps_the_role_volumes() {
        let r = rig();
        let image = staged_image(&r, &[1]).await;
        let dead =
            DeadSources(FileSources::new(r._dir.path().join("dead"), r.store.clone()).unwrap());
        let volumes =
            SessionVolumes::new(r.disk.clone(), r.store.clone()).with_sources(Arc::new(dead));
        let sandbox = SandboxId::new();
        volumes
            .provision(sandbox, lazy_fork(image), None)
            .await
            .unwrap();
        let before = names(&r).await;

        assert!(volumes.release(sandbox).await.is_err());
        let report = volumes.purge(sandbox).await.unwrap();

        assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
        assert_eq!(names(&r).await, before);
        r.disk
            .open_clone(&live_name(sandbox, DiskRole::Root))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn two_lineages_for_one_role_are_an_error_and_keep_their_volumes() {
        let r = rig();
        let sandbox = SandboxId::new();
        let (a, b) = (ManifestRef::new(), ManifestRef::new());
        for m in [a, b] {
            r.disk
                .create(&checkpoint_name(sandbox, DiskRole::Root, m), CHUNK)
                .await
                .unwrap();
        }
        let before = names(&r).await;

        let err = r
            .volumes
            .checkpoint_of(sandbox, DiskRole::Root)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        let err = r.volumes.release(sandbox).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert_eq!(names(&r).await, before);
    }
}
