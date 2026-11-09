//! The snapshot post phase: publish what a capture took. The periodic
//! checkpoint, the Snapshot RPC and the eviction finalize all run it.
//!
//! Three legs run at the same time: the disk publish, the memory publish
//! followed by the sidecar upload (the memory publish patches the
//! sidecar), and the `state.bin` upload with the bundle publish. Each leg
//! is idempotent, so a caller that failed can run the phase again from
//! the same staging dir.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use engram_chunk_store::{ChunkStore, Manifest};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::AuxBundleRef;
use engram_core::types::snapshot::DiskRole;
use engram_core::{SandboxError, SandboxId, SessionId, SnapshotId};
use engram_fill::{Class, FillEngine};
use engram_host_core::session_volumes::PendingCheckpoint;
use engram_host_core::{HostFs, SessionVolumes};
use tracing::Instrument as _;

use crate::eviction_finalize::{DiffExtents, PendingDisk};

/// A chunk store and the host's fill engine over it. Chunk writes go to
/// the store at its class; chunk reads go through the engine at the same
/// class.
#[derive(Clone)]
pub(crate) struct Chunks {
    pub store: ChunkStore,
    pub fill: FillEngine,
}

impl Chunks {
    /// The engine's store, admitted at its default class.
    pub(crate) fn new(fill: FillEngine) -> Self {
        Self {
            store: fill.store().clone(),
            fill,
        }
    }

    /// These chunks with transfers admitted at `class`.
    pub(crate) fn with_class(self, class: Class) -> Self {
        Self {
            store: self.store.with_class(class),
            fill: self.fill,
        }
    }
}

/// The stores the post phase writes to.
pub(crate) struct PostPhase<'a> {
    pub volumes: Option<&'a SessionVolumes>,
    pub chunks: Option<&'a Chunks>,
    pub bundle_dir: &'a Path,
    pub bundle_file_ext: &'static str,
    pub fs: &'a dyn HostFs,
}

/// The disk checkpoints a capture took.
pub(crate) enum Disks {
    /// Held by the caller. A failed publish discards the ones after it.
    Held(Vec<PendingCheckpoint>),
    /// Named by a durable record: each is found on the session disk.
    Named {
        sandbox: SandboxId,
        pending: Vec<PendingDisk>,
    },
}

/// The memory artifact a capture left in the staging dir.
pub(crate) enum Memory {
    /// `memory.diff` over the chain's previous manifest. `prev` is that
    /// manifest when the caller has it; otherwise it is fetched. The
    /// publish refuses a diff whose extents differ from `extents`.
    Diff {
        prev_ref: ManifestRef,
        prev: Option<Manifest>,
        extents: Option<DiffExtents>,
    },
    /// `memory.bin`, the whole guest memory. A missing file is a lost
    /// file: a caller whose backend writes no guest memory passes no
    /// `Memory`.
    Full,
}

/// One run of the post phase. A `None` leg was published by an earlier
/// run.
pub(crate) struct Work<'w> {
    pub snapshot_id: SnapshotId,
    pub dest: &'w Path,
    pub disks: Option<Disks>,
    pub memory: Option<Memory>,
    pub blobs: bool,
    /// Stamped into the sidecar as the working-set trace key.
    pub trace_lineage: Option<SessionId>,
    pub aux_bundles: &'w [AuxBundleRef],
    /// The `source` label of the full re-chunk metrics.
    pub source: &'static str,
}

/// What one run published.
pub(crate) struct Published {
    pub root: Option<ManifestRef>,
    pub swap: Option<ManifestRef>,
    pub memory: Option<PublishedMemory>,
    pub blobs: Option<BlobKeys>,
}

/// Each leg's own result from one run.
pub(crate) struct Ran {
    pub disks: Result<(Option<ManifestRef>, Option<ManifestRef>), SandboxError>,
    pub memory: Result<Option<PublishedMemory>, SandboxError>,
    pub blobs: Result<Option<BlobKeys>, SandboxError>,
    pub seconds: LegSeconds,
}

impl Ran {
    /// What the run published, or the first leg error.
    pub(crate) fn into_published(self) -> Result<Published, SandboxError> {
        let (root, swap) = self.disks?;
        Ok(Published {
            root,
            swap,
            memory: self.memory?,
            blobs: self.blobs?,
        })
    }
}

pub(crate) struct PublishedMemory {
    pub manifest_ref: ManifestRef,
    /// The next chain manifest; `None` for a full capture.
    pub next: Option<Manifest>,
    /// The diff's dirty bytes; `None` for a full capture.
    pub dirty_bytes: Option<u64>,
    /// The memory file the publish consumed. The caller deletes it once
    /// nothing re-runs the memory leg.
    pub consumed: PathBuf,
}

pub(crate) struct BlobKeys {
    pub state: String,
    pub sidecar: String,
}

/// Wall-clock of each leg that ran.
pub(crate) struct LegSeconds {
    pub disk: Option<f64>,
    pub memory: Option<f64>,
    pub blobs: Option<f64>,
}

impl PostPhase<'_> {
    /// Run every leg; fail if any leg failed.
    pub(crate) async fn run(&self, work: Work<'_>) -> Result<Published, SandboxError> {
        self.run_legs(work).await?.into_published()
    }

    /// Run every leg and return each leg's own result, so a caller can
    /// keep what one leg published when another leg failed. `Err` means
    /// that no leg ran.
    pub(crate) async fn run_legs(&self, work: Work<'_>) -> Result<Ran, SandboxError> {
        let Work {
            snapshot_id,
            dest,
            disks,
            memory,
            blobs,
            trace_lineage,
            aux_bundles,
            source,
        } = work;
        let state_path = dest.join("state.bin");
        let sidecar_path = dest.join("manifest.json");
        let blob = self
            .chunks
            .map(|chunks| chunks.store.blob_storage())
            .filter(|_| blobs);
        if blob.is_some() {
            check_restore_files(&state_path, &sidecar_path).await?;
        }
        let state_key = engram_chunk_store::snapshot_blob::state_blob_key(snapshot_id);
        let sidecar_key = engram_chunk_store::snapshot_blob::sidecar_blob_key(snapshot_id);

        let disk_leg = async {
            let Some(disks) = disks else {
                return (Ok((None, None)), None);
            };
            let start = crate::time_source::metrics_now();
            let published = self
                .publish_disks(disks)
                .instrument(tracing::info_span!("finish.disk_publish"))
                .await;
            (published, Some(start.elapsed().as_secs_f64()))
        };
        // The sidecar upload follows the memory publish, which patches
        // the sidecar. Its result belongs to the blobs.
        let memory_leg = async {
            let start = crate::time_source::metrics_now();
            let (published, memory_seconds) = match memory {
                Some(memory) => {
                    let published = async {
                        let published = self.publish_memory(dest, memory, source).await?;
                        if let Some(m) = &published {
                            patch_fc_manifest_memory_ref(
                                self.fs,
                                &sidecar_path,
                                m.manifest_ref,
                                trace_lineage,
                            )
                            .await?;
                        }
                        Ok::<_, SandboxError>(published)
                    }
                    .await;
                    (published, Some(start.elapsed().as_secs_f64()))
                }
                None => (Ok(None), None),
            };
            let sidecar = match (&published, blob.as_ref()) {
                (Err(_), _) | (_, None) => Ok(()),
                (Ok(_), Some(blob)) => engram_chunk_store::snapshot_blob::upload_file(
                    blob.as_ref(),
                    &sidecar_key,
                    &sidecar_path,
                )
                .instrument(tracing::info_span!("finish.sidecar_upload"))
                .await
                .map(drop)
                .map_err(|e| SandboxError::Snapshot(format!("upload sidecar.json: {e}"))),
            };
            (published, memory_seconds, sidecar)
        };
        let state_leg = async {
            let Some(blob) = blob.as_ref() else {
                return (Ok(()), None);
            };
            let start = crate::time_source::metrics_now();
            let uploaded = async {
                engram_chunk_store::snapshot_blob::upload_file(
                    blob.as_ref(),
                    &state_key,
                    &state_path,
                )
                .instrument(tracing::info_span!("finish.blob_upload"))
                .await
                .map_err(|e| SandboxError::Snapshot(format!("upload state.bin: {e}")))?;
                // A pin nothing can satisfy is worse than a retried
                // capture, so a failed bundle publish fails the phase.
                if !aux_bundles.is_empty() {
                    crate::bundles::BundleStore::new(
                        blob.clone(),
                        self.bundle_dir.to_path_buf(),
                        self.bundle_file_ext,
                    )
                    .publish(aux_bundles)
                    .instrument(tracing::info_span!("finish.bundle_publish"))
                    .await?;
                }
                Ok::<_, SandboxError>(())
            }
            .await;
            (uploaded, Some(start.elapsed().as_secs_f64()))
        };
        // `join!`, not `try_join!`: a failed leg must not cancel a disk
        // publish half way, which would leak its pending checkpoints.
        let ((disks, disk_seconds), (memory, memory_seconds, sidecar), (state, state_seconds)) =
            tokio::join!(disk_leg, memory_leg, state_leg);
        let blobs = match blob {
            None => Ok(None),
            Some(_) => match (&memory, state, sidecar) {
                (Err(_), _, _) => Err(SandboxError::Snapshot(
                    "the sidecar was not uploaded: the memory publish failed".into(),
                )),
                (Ok(_), Err(e), _) | (Ok(_), Ok(()), Err(e)) => Err(e),
                (Ok(_), Ok(()), Ok(())) => Ok(Some(BlobKeys {
                    state: state_key,
                    sidecar: sidecar_key,
                })),
            },
        };
        Ok(Ran {
            disks,
            memory,
            blobs,
            seconds: LegSeconds {
                disk: disk_seconds,
                memory: memory_seconds,
                blobs: state_seconds,
            },
        })
    }

    async fn publish_disks(
        &self,
        disks: Disks,
    ) -> Result<(Option<ManifestRef>, Option<ManifestRef>), SandboxError> {
        let mut root = None;
        let mut swap = None;
        let no_volumes = || {
            SandboxError::Snapshot(
                "a capture has pending disk checkpoints but this host has no session disk".into(),
            )
        };
        match disks {
            Disks::Held(pending) => {
                if pending.is_empty() {
                    return Ok((None, None));
                }
                let volumes = self.volumes.ok_or_else(no_volumes)?;
                let mut pending = pending.into_iter();
                while let Some(checkpoint) = pending.next() {
                    let role = checkpoint.role();
                    let published = match volumes.publish(checkpoint).await {
                        Ok(published) => published,
                        Err(error) => {
                            for rest in pending {
                                crate::pooled_backend::discard_pending(volumes, rest).await;
                            }
                            return Err(SandboxError::Snapshot(format!(
                                "{role} disk publish: {error}"
                            )));
                        }
                    };
                    tracing::info!(%role, manifest = %published, "disk checkpoint published");
                    match role {
                        DiskRole::Root => root = Some(published),
                        DiskRole::Swap => swap = Some(published),
                    }
                }
            }
            Disks::Named { sandbox, pending } => {
                if pending.is_empty() {
                    return Ok((None, None));
                }
                let volumes = self.volumes.ok_or_else(no_volumes)?;
                for PendingDisk { role, previous } in pending {
                    let published = publish_named(volumes, sandbox, role, previous).await?;
                    match role {
                        DiskRole::Root => root = Some(published),
                        DiskRole::Swap => swap = Some(published),
                    }
                }
            }
        }
        Ok((root, swap))
    }

    async fn publish_memory(
        &self,
        dest: &Path,
        memory: Memory,
        source: &'static str,
    ) -> Result<Option<PublishedMemory>, SandboxError> {
        let Some(Chunks {
            store: chunk_store,
            fill,
        }) = self.chunks
        else {
            return Ok(None);
        };
        match memory {
            Memory::Diff {
                prev_ref,
                prev,
                extents,
            } => {
                // A diff create always writes `memory.diff`; a missing one
                // was lost, and publishing without it would drop the
                // pages its dirty set consumed.
                let diff_path = dest.join("memory.diff");
                let prev = match prev {
                    Some(prev) => prev,
                    None => chunk_store.get_manifest(prev_ref).await.map_err(|e| {
                        SandboxError::Snapshot(format!("get chain-prev manifest {prev_ref}: {e}"))
                    })?,
                };
                let ranges = std::fs::File::open(&diff_path)
                    .and_then(|f| engram_host_core::data_extents(&f))
                    .map_err(|e| SandboxError::Snapshot(format!("dirty ranges: {e}")))?;
                // A page lost in a crash reads as a hole, so it would look
                // clean and keep the previous chunk.
                if let Some(expected) = extents {
                    let found = DiffExtents::of(&ranges);
                    if found != expected {
                        return Err(SandboxError::Snapshot(format!(
                            "memory.diff holds {found} but the capture recorded {expected}: \
                             the file lost pages"
                        )));
                    }
                }
                let dirty_bytes: u64 = ranges.iter().map(|(_, len)| *len).sum();
                let next = chunk_store
                    .update_for_dirty_ranges_sparse_from(
                        &prev,
                        &diff_path,
                        &ranges,
                        Arc::new(fill.clone()),
                    )
                    .instrument(tracing::info_span!(
                        "finish.memory_rechunk",
                        dirty_bytes,
                        dirty_ranges = ranges.len()
                    ))
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("sparse re-chunk: {e}")))?;
                let next_ref = prev_ref.next_version();
                let manifest_ref = publish_manifest_collision_safe(chunk_store, next_ref, &next)
                    .instrument(tracing::info_span!("finish.manifest_put"))
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("put manifest {next_ref}: {e}")))?;
                tracing::info!(
                    manifest = %manifest_ref,
                    dirty_ranges = ranges.len(),
                    "diff checkpoint re-chunked",
                );
                Ok(Some(PublishedMemory {
                    manifest_ref,
                    next: Some(next),
                    dirty_bytes: Some(dirty_bytes),
                    consumed: diff_path,
                }))
            }
            Memory::Full => {
                let mem_path = dest.join("memory.bin");
                let manifest_ref = chunk_memory_to_store(chunk_store, &mem_path, source)
                    .instrument(tracing::info_span!("finish.memory_chunk"))
                    .await?;
                tracing::info!(manifest = %manifest_ref, "full memory chunked");
                Ok(Some(PublishedMemory {
                    manifest_ref,
                    next: None,
                    dirty_bytes: None,
                    consumed: mem_path,
                }))
            }
        }
    }
}

/// Publish a pending checkpoint found by name. One that is gone was
/// published by an earlier run that died before its caller recorded it
/// only if the disk's checkpoint is newer than the one it was taken
/// over; otherwise it was lost, and the publish fails instead of
/// answering with the older checkpoint.
async fn publish_named(
    volumes: &SessionVolumes,
    sandbox: SandboxId,
    role: DiskRole,
    previous: ManifestRef,
) -> Result<ManifestRef, SandboxError> {
    match volumes.pending(sandbox, role).await {
        Ok(pending) => volumes
            .publish(pending)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("publish {role} disk: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let current = volumes.checkpoint_of(sandbox, role).await.map_err(|e| {
                SandboxError::Snapshot(format!(
                    "{role} disk has no pending checkpoint and no checkpoint: {e}"
                ))
            })?;
            if current.manifest_id != previous.manifest_id || current.version <= previous.version {
                return Err(SandboxError::Snapshot(format!(
                    "the pending {role} disk checkpoint is gone and was never published \
                     (the disk holds {current}, taken over {previous})"
                )));
            }
            Ok(current)
        }
        Err(e) => Err(SandboxError::Snapshot(format!(
            "open pending {role} disk checkpoint: {e}"
        ))),
    }
}

/// Publish deterministic content at `deterministic_ref`. An identical
/// earlier publish there is reused; different content there is kept,
/// and this content goes to the lineage's next free version.
async fn publish_manifest_collision_safe(
    chunk_store: &ChunkStore,
    deterministic_ref: ManifestRef,
    manifest: &Manifest,
) -> Result<ManifestRef, engram_chunk_store::ChunkStoreError> {
    const MAX_RETRIES: u32 = 32;
    let mut attempt_ref = deterministic_ref;
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        match chunk_store.put_manifest(attempt_ref, manifest).await {
            Ok(()) => return Ok(attempt_ref),
            Err(engram_chunk_store::ChunkStoreError::VersionConflict {
                latest,
                attempted,
                manifest_id,
            }) => {
                if attempt_ref == deterministic_ref && attempted == deterministic_ref.version {
                    let existing = chunk_store.get_manifest(deterministic_ref).await?;
                    if existing == *manifest {
                        return Ok(deterministic_ref);
                    }
                }
                if attempts >= MAX_RETRIES {
                    return Err(engram_chunk_store::ChunkStoreError::VersionConflict {
                        latest,
                        attempted,
                        manifest_id,
                    });
                }
                attempt_ref = ManifestRef {
                    manifest_id,
                    version: latest + 1,
                };
            }
            Err(e) => return Err(e),
        }
    }
}

/// Chunk a whole `memory.bin` into the store and publish its manifest
/// under a fresh id. The store's cache keeps the chunks on the
/// capturing host.
pub(crate) async fn chunk_memory_to_store(
    chunk_store: &ChunkStore,
    memory_bin: &Path,
    // The `source` label of the re-chunk metrics: which capture flavor
    // paid for this full-image scan.
    source: &'static str,
) -> Result<ManifestRef, SandboxError> {
    let res = chunk_store
        .chunk_file_into(
            memory_bin,
            engram_chunk_store::ManifestKind::Memory,
            None,
            None,
        )
        .await;
    let outcome = if res.is_ok() { "success" } else { "error" };
    if let Ok((_, stats)) = &res {
        for (phase, seconds) in [
            ("scan", stats.scan_seconds),
            ("upload", stats.flush_seconds),
        ] {
            metrics::histogram!(
                crate::metrics::RECHUNK_SECONDS,
                "phase" => phase,
                "source" => source,
                "outcome" => outcome,
            )
            .record(seconds);
        }
        metrics::counter!(crate::metrics::RECHUNK_BYTES_SCANNED_TOTAL, "source" => source)
            .increment(stats.bytes_scanned);
        metrics::counter!(crate::metrics::RECHUNK_BYTES_UPLOADED_TOTAL, "source" => source)
            .increment(stats.bytes_uploaded);
        tracing::info!(
            memory_bin = %memory_bin.display(),
            source,
            scan_ms = (stats.scan_seconds * 1000.0) as u64,
            upload_ms = (stats.flush_seconds * 1000.0) as u64,
            bytes_scanned = stats.bytes_scanned,
            chunks_uploaded = stats.chunks_uploaded,
            bytes_uploaded = stats.bytes_uploaded,
            "full memory re-chunk phase breakdown",
        );
    }
    let (manifest, _) = res.map_err(|e| {
        SandboxError::Snapshot(format!("chunk memory.bin {}: {e}", memory_bin.display()))
    })?;
    let manifest_ref = ManifestRef::new();
    chunk_store
        .put_manifest(manifest_ref, &manifest)
        .await
        .map_err(|e| {
            SandboxError::Snapshot(format!(
                "put_manifest {manifest_ref} for {}: {e}",
                memory_bin.display(),
            ))
        })?;
    Ok(manifest_ref)
}

/// Set the `memory_manifest` field, and the `trace_lineage_id` field when
/// given, on the FC sidecar JSON. The patch works on the JSON value: the
/// on-disk shape is the contract with the FC backend, which reads it with
/// `#[serde(default)]`.
///
/// The patched file replaces the old one atomically and durably, so a
/// crash never leaves a torn sidecar for a redrive or a restore.
pub(crate) async fn patch_fc_manifest_memory_ref(
    fs: &dyn HostFs,
    manifest_json: &Path,
    memory_manifest: ManifestRef,
    trace_lineage: Option<SessionId>,
) -> Result<(), SandboxError> {
    let bytes = fs.read(manifest_json).await.map_err(|e| {
        SandboxError::Snapshot(format!(
            "read fc manifest.json {}: {e}",
            manifest_json.display(),
        ))
    })?;
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
        SandboxError::Snapshot(format!(
            "parse fc manifest.json {}: {e}",
            manifest_json.display(),
        ))
    })?;
    let obj = value.as_object_mut().ok_or_else(|| {
        SandboxError::Snapshot(format!(
            "fc manifest.json {} root is not an object",
            manifest_json.display(),
        ))
    })?;
    obj.insert(
        "memory_manifest".into(),
        serde_json::to_value(memory_manifest)
            .map_err(|e| SandboxError::Snapshot(format!("serialize memory_manifest: {e}")))?,
    );
    // The session id keys the working-set trace, so the next resume's
    // prefault replay finds it whatever the memory manifest id is.
    if let Some(session_id) = trace_lineage {
        obj.insert(
            "trace_lineage_id".into(),
            serde_json::to_value(session_id)
                .map_err(|e| SandboxError::Snapshot(format!("serialize trace_lineage_id: {e}")))?,
        );
    }
    let bytes = serde_json::to_vec_pretty(&value)
        .map_err(|e| SandboxError::Snapshot(format!("serialize patched manifest.json: {e}")))?;
    crate::durable_record::replace(fs, manifest_json, &bytes)
        .await
        .map_err(|e| {
            SandboxError::Snapshot(format!(
                "write patched manifest.json {}: {e}",
                manifest_json.display(),
            ))
        })
}

/// `state.bin` must hold bytes and the sidecar must be a JSON object:
/// a crash can leave either empty or torn, and the post phase must not
/// upload such a file as the snapshot's restore input.
async fn check_restore_files(state: &Path, sidecar: &Path) -> Result<(), SandboxError> {
    match tokio::fs::metadata(state).await {
        Ok(meta) if meta.len() > 0 => {}
        Ok(_) => {
            return Err(SandboxError::Snapshot(format!(
                "{} is empty",
                state.display()
            )))
        }
        Err(e) => {
            return Err(SandboxError::Snapshot(format!(
                "stat {}: {e}",
                state.display()
            )))
        }
    }
    let bytes = tokio::fs::read(sidecar)
        .await
        .map_err(|e| SandboxError::Snapshot(format!("read {}: {e}", sidecar.display())))?;
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(serde_json::Value::Object(_)) => Ok(()),
        Ok(_) => Err(SandboxError::Snapshot(format!(
            "{} root is not an object",
            sidecar.display()
        ))),
        Err(e) => Err(SandboxError::Snapshot(format!(
            "parse {}: {e}",
            sidecar.display()
        ))),
    }
}

/// The capture files that must reach the disk before a durable record
/// names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncScope {
    /// `state.bin` only: the memory file is chunked and retired, so the
    /// chunk store holds its durable copy, and the post phase replaces
    /// the sidecar durably.
    State,
    /// Every capture file: a redrive reads them back from the staging dir.
    All,
}

impl SyncScope {
    fn files(self) -> &'static [&'static str] {
        match self {
            SyncScope::State => &["state.bin"],
            SyncScope::All => &["state.bin", "memory.diff", "memory.bin", "manifest.json"],
        }
    }
}

/// fsync the capture files that exist in `dest`, then the dir, then its
/// parent, which holds the new dir's own entry. The FC backend resumes
/// the guest before its files reach the disk (`SandboxBackend::snapshot`'s
/// contract).
pub(crate) async fn sync_capture(
    fs: &dyn HostFs,
    dest: &Path,
    scope: SyncScope,
) -> std::io::Result<()> {
    for name in scope.files() {
        match fs.sync_file(&dest.join(name)).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    fs.sync_dir(dest).await?;
    match dest.parent() {
        Some(parent) => fs.sync_dir(parent).await,
        None => Ok(()),
    }
}

/// Retire a consumed memory file: rename it away and fsync the dir, then
/// delete it off the caller's path (unlinking a multi-GiB file takes long
/// enough to show). After `Ok`, a crash cannot bring the file back under
/// its own name, so no restore can read an un-synced memory file as whole.
pub(crate) async fn retire_consumed(fs: Arc<dyn HostFs>, path: PathBuf) -> std::io::Result<()> {
    let mut doomed = path.clone().into_os_string();
    doomed.push(".consumed");
    let doomed = PathBuf::from(doomed);
    match fs.rename(&path, &doomed).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    if let Some(dir) = path.parent() {
        fs.sync_dir(dir).await?;
    }
    tokio::spawn(async move {
        if let Err(e) = fs.remove_file(&doomed).await {
            tracing::warn!(path = %doomed.display(), error = %e, "delete of a consumed memory file failed");
        }
    });
    Ok(())
}
