//! ADR 0080 §C — host-side materialization of a **standard**
//! OCI/Docker image into a bootable engram rootfs.
//!
//! Pipeline (each stage its own module, each a clean seam):
//!
//! 1. [`pull`] — resolve the manifest for a platform (manifest lists /
//!    OCI indexes handled), extract the config blob's `ENV`/`WORKDIR`
//!    as [`OciRuntimeDefaults`], stream layer blobs to scratch. Fails
//!    loud on unknown layer mediaTypes before downloading bytes.
//! 2. [`flatten`] — whiteout-aware layer application into one tree
//!    (`.wh.`, opaque dirs, hardlinks, symlinks, setuid, xattrs, the
//!    unprivileged-ownership sidecar, zip-slip rejection).
//! 3. [`inject`] — write the stage-1 init shim (the ONE engrams file
//!    in the rootfs).
//! 4. [`ext4`] — deterministic mke2fs pack (fixed UUID/hash-seed +
//!    `SOURCE_DATE_EPOCH` + a tree-side mtime clamp).
//! 5. chunk — `chunk_file` into the content-addressed store; the
//!    manifest ref is **content-derived** ([`Manifest::content_ref`]),
//!    so re-materializing unchanged content reproduces the same ref
//!    and base-snapshot reuse keeps working (ADR 0036).
//!
//! Everything transient lives under one caller-supplied scratch dir
//! and is scrubbed on success AND error (drop guard). Phase 3b wraps
//! [`Materializer::materialize`] in the `MaterializeImage` host RPC;
//! this crate carries no wire types.
//!
//! [`Manifest::content_ref`]: engram_chunk_store::Manifest::content_ref
//! [`OciRuntimeDefaults`]: engram_core::types::image::OciRuntimeDefaults

pub mod ext4;
pub mod flatten;
pub mod inject;
pub mod pull;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engram_chunk_store::{ChunkStore, ManifestKind, ManifestRef};
use engram_core::types::image::OciRuntimeDefaults;

pub use ext4::{
    clamp_mtimes, recommended_size, recursive_size, Ext4Error, Ext4Packer, Mke2fsPacker,
};
pub use flatten::{
    apply_layer, default_write_concurrency, ChannelReader, EntryMeta, FlattenError, FlattenStats,
    Flattener, SkippedXattr, TreeMetadata,
};
pub use inject::{inject_init, InitInjection, Transport, DEFAULT_INIT_SHIM};
pub use pull::{
    download_layer, layer_compression, pull_image, resolve_image, LayerCompression, LayerPlan,
    Platform, PullError, PulledImage, PulledLayer, ResolvedImage,
};

/// Download look-ahead for the pull/flatten pipeline: up to this many
/// layer downloads in flight, delivered to the flattener in manifest
/// order (`buffered`, not `buffer_unordered` — layers MUST apply in
/// order). Scratch holds at most this + the layer being flattened.
const PULL_LOOKAHEAD: usize = 3;

/// Scratch headroom to budget for one materialize, as a multiple of
/// the image's compressed size: compressed layers (~1×) + the
/// flattened tree + the packed ext4 overlap at the peak. The ~2.5×
/// figure is the ADR's planning hint for 3b's disk-veto/budget
/// integration — an estimate, not a guarantee (a highly-compressed
/// image can expand further; the enable-time size cap bounds the
/// blast radius).
pub fn estimated_peak_scratch_bytes(compressed_image_bytes: u64) -> u64 {
    compressed_image_bytes.saturating_mul(5) / 2
}

/// Result of one materialize: everything phase 3b's RPC reply needs.
#[derive(Clone, Debug)]
pub struct Materialized {
    /// Content-derived ref of the packed disk's chunk manifest,
    /// committed to the chunk store (same identity a deterministic
    /// re-run reproduces).
    pub disk_manifest: ManifestRef,
    /// Dockerfile `ENV` + `WORKDIR` from the image config blob —
    /// persisted by the enable pipeline, merged under the admin
    /// `ImageConfig` at session create.
    pub oci_defaults: OciRuntimeDefaults,
    /// Digest (`sha256:<hex>`) of the platform-resolved docker image
    /// manifest that was materialized — the enable row's
    /// `manifest_digest` (keeps digest-pinning for capture).
    pub manifest_digest: String,
    /// Size of the packed ext4 in bytes.
    pub ext4_size_bytes: u64,
}

#[derive(Debug)]
pub enum MaterializeError {
    Pull(PullError),
    Flatten(FlattenError),
    Ext4(Ext4Error),
    ChunkStore(engram_chunk_store::ChunkStoreError),
    Io(std::io::Error),
}

impl std::fmt::Display for MaterializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pull(e) => write!(f, "pull: {e}"),
            Self::Flatten(e) => write!(f, "flatten: {e}"),
            Self::Ext4(e) => write!(f, "ext4 pack: {e}"),
            Self::ChunkStore(e) => write!(f, "chunk store: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for MaterializeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Pull(e) => Some(e),
            Self::Flatten(e) => Some(e),
            Self::Ext4(e) => Some(e),
            Self::ChunkStore(e) => Some(e),
            Self::Io(e) => Some(e),
        }
    }
}

impl From<PullError> for MaterializeError {
    fn from(e: PullError) -> Self {
        Self::Pull(e)
    }
}
impl From<FlattenError> for MaterializeError {
    fn from(e: FlattenError) -> Self {
        Self::Flatten(e)
    }
}
impl From<Ext4Error> for MaterializeError {
    fn from(e: Ext4Error) -> Self {
        Self::Ext4(e)
    }
}
impl From<engram_chunk_store::ChunkStoreError> for MaterializeError {
    fn from(e: engram_chunk_store::ChunkStoreError) -> Self {
        Self::ChunkStore(e)
    }
}
impl From<std::io::Error> for MaterializeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Scrubs the per-run scratch subdir on drop — success, error, and
/// panic all leave the caller's scratch dir clean (ADR 0080 host
/// safeguard: "scrub on all exit paths").
struct ScratchGuard {
    dir: PathBuf,
}

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(dir = %self.dir.display(), error = %e, "scratch scrub failed");
            }
        }
    }
}

/// The materializer: OCI client (auth resolved by the caller's
/// [`engram_oci::RegistryAuthResolver`]) + an ext4 packer + the init
/// injection to stamp into every produced rootfs.
pub struct Materializer {
    oci: engram_oci::OciClient,
    packer: Arc<dyn Ext4Packer>,
    init: InitInjection,
}

impl Materializer {
    /// Production constructor: real `mke2fs` packer.
    pub fn new(oci: engram_oci::OciClient, init: InitInjection) -> Self {
        Self {
            oci,
            packer: Arc::new(Mke2fsPacker::default()),
            init,
        }
    }

    /// Custom packer — tests record / inject failures instead of
    /// running real mke2fs.
    pub fn with_packer(
        oci: engram_oci::OciClient,
        init: InitInjection,
        packer: Arc<dyn Ext4Packer>,
    ) -> Self {
        Self { oci, packer, init }
    }

    /// Run the full pipeline for `image_uri` on `platform`. All
    /// transient state lives under a fresh subdir of `scratch_dir`
    /// and is scrubbed whatever the outcome; the only durable outputs
    /// are the chunks + manifest committed to `chunk_store` and the
    /// returned [`Materialized`].
    ///
    /// `progress` (phase 3b): best-effort stage frames (`try_send`),
    /// one per pipeline stage transition PLUS intra-stage chunk-window
    /// progress frames (stage `Chunk` with `chunks_done/chunks_total`
    /// set, one per flushed upload window — the enable UI's progress
    /// bar). The `MaterializeImage` RPC forwards them coord-ward; the
    /// keepalive cadence (≤30 s) is the CALLER's job (it re-sends the
    /// last frame). Consumers must tolerate repeated frames for the
    /// same stage. `None` = silent (tests, the bake).
    pub async fn materialize(
        &self,
        image_uri: &str,
        platform: Platform,
        scratch_dir: &Path,
        chunk_store: &ChunkStore,
        progress: Option<tokio::sync::mpsc::Sender<engram_core::types::MaterializeProgress>>,
    ) -> Result<Materialized, MaterializeError> {
        use engram_core::types::{MaterializeProgress, MaterializeStage};
        let report = |stage: MaterializeStage, detail: Option<String>| {
            if let Some(tx) = &progress {
                let _ = tx.try_send(MaterializeProgress {
                    stage,
                    detail,
                    chunks_done: None,
                    chunks_total: None,
                });
            }
        };

        tokio::fs::create_dir_all(scratch_dir).await?;
        let work = scratch_dir.join(format!("materialize-{}", uuid::Uuid::new_v4().simple()));
        let _guard = ScratchGuard { dir: work.clone() };

        // 1+2. Pull/flatten PIPELINE (ADR 0088 addendum): downloads run
        // up to PULL_LOOKAHEAD-wide with in-order readiness (`buffered`)
        // and feed the flatten consumer as each layer lands — pull
        // wall-time hides under the flatten. Each layer file is deleted
        // as soon as it's applied, so scratch holds ≤ lookahead + one
        // layer at the peak (LESS than the old all-layers-then-flatten).
        report(MaterializeStage::Pull, Some(format!("{platform}")));
        let pipeline_started = std::time::Instant::now();
        let resolved = pull::resolve_image(&self.oci, image_uri, platform).await?;
        let layer_count = resolved.layers.len();
        let compressed_bytes = resolved.compressed_bytes;
        let layers_dir = work.join("layers");
        tokio::fs::create_dir_all(&layers_dir).await?;
        let rootfs = work.join("rootfs");
        tokio::fs::create_dir_all(&rootfs).await?;

        // Bounded handoff: the driver blocks once the flattener is
        // 2 layers behind (plus lookahead in flight).
        let (layer_tx, mut layer_rx) = tokio::sync::mpsc::channel::<PulledLayer>(2);

        // The driver is spawned with owned captures (OciClient is
        // Clone): a reference-capturing async block joined under the
        // caller's #[instrument] span trips rustc's
        // Send-not-general-enough inference.
        let pull_driver =
            {
                let oci = self.oci.clone();
                let image_uri = image_uri.to_string();
                let layers_dir = layers_dir.clone();
                let plans = resolved.layers.clone();
                tokio::spawn(async move {
                    use futures::StreamExt as _;
                    let started = std::time::Instant::now();
                    let mut stream =
                        futures::stream::iter(plans.into_iter().map(|plan| {
                            // Fully owned per-download captures: reference-holding
                            // closures under buffered() trip rustc's
                            // FnOnce-not-general-enough inference.
                            let oci = oci.clone();
                            let image_uri = image_uri.clone();
                            let layers_dir = layers_dir.clone();
                            async move {
                                pull::download_layer(&oci, &image_uri, &plan, &layers_dir).await
                            }
                        }))
                        .buffered(PULL_LOOKAHEAD);
                    while let Some(pulled) = stream.next().await {
                        let pulled = pulled?;
                        if layer_tx.send(pulled).await.is_err() {
                            // Consumer died (flatten error) — its error wins;
                            // just stop downloading.
                            break;
                        }
                    }
                    Ok::<u64, PullError>(started.elapsed().as_millis() as u64)
                })
            };

        let flatten_consumer = {
            let rootfs = rootfs.clone();
            let progress = progress.clone();
            tokio::task::spawn_blocking(
                move || -> Result<(TreeMetadata, FlattenStats, u64), MaterializeError> {
                    let mut meta = TreeMetadata::default();
                    // One engine across all layers: the fd-scoped write pool
                    // and the memoized resolve cache are reused layer to layer.
                    let mut flattener =
                        flatten::Flattener::new(&rootfs, flatten::default_write_concurrency())?;
                    let mut layer_result: Result<(), MaterializeError> = Ok(());
                    let mut applied = 0usize;
                    let mut flatten_busy_ms = 0u64;
                    while let Some(layer) = layer_rx.blocking_recv() {
                        applied += 1;
                        // Frame per layer (keepalive-coalesced coord-side;
                        // never re-emits Pull after the first Flatten).
                        if let Some(tx) = &progress {
                            let _ = tx.try_send(engram_core::types::MaterializeProgress {
                                stage: MaterializeStage::Flatten,
                                detail: Some(format!(
                                "layer {applied}/{layer_count}, {compressed_bytes} compressed bytes"
                            )),
                                chunks_done: None,
                                chunks_total: None,
                            });
                        }
                        // Per-layer timing + entry delta: a slow layer with a
                        // huge entry delta is syscall-bound tiny-file creation;
                        // slow with a small delta is decompression.
                        let layer_started = std::time::Instant::now();
                        let entries_before = meta.len();
                        let file = match std::fs::File::open(&layer.path) {
                            Ok(f) => f,
                            Err(e) => {
                                layer_result = Err(e.into());
                                break;
                            }
                        };
                        let reader = std::io::BufReader::new(file);
                        // Decompression runs on its own thread (ChannelReader)
                        // so inflate overlaps the reader's syscall work.
                        let decode = || -> Result<flatten::ChannelReader, std::io::Error> {
                            let decoder: Box<dyn std::io::Read + Send> = match layer.compression {
                                LayerCompression::Gzip => {
                                    Box::new(flate2::read::GzDecoder::new(reader))
                                }
                                // C-backed zstd (ADR 0088 addendum rollout
                                // fix): ruzstd's single-threaded pure-Rust
                                // decode WAS the flatten bottleneck for
                                // zstd-layered images — dev-brain's 3.5 GB
                                // compressed layer held the fd-worker pool
                                // idle behind it for tens of minutes.
                                LayerCompression::Zstd => Box::new(
                                    zstd::stream::read::Decoder::with_buffer(reader)
                                        .map_err(std::io::Error::other)?,
                                ),
                                LayerCompression::None => Box::new(reader),
                            };
                            flatten::ChannelReader::spawn(decoder)
                        };
                        let piped = match decode() {
                            Ok(p) => p,
                            Err(e) => {
                                layer_result = Err(e.into());
                                break;
                            }
                        };
                        if let Err(e) = flattener.apply_layer(&mut meta, piped) {
                            layer_result = Err(e.into());
                            break;
                        }
                        tracing::debug!(
                            digest = %layer.digest,
                            compression = ?layer.compression,
                            entries = meta.len() - entries_before,
                            elapsed_ms = layer_started.elapsed().as_millis() as u64,
                            "layer applied to tree"
                        );
                        flatten_busy_ms += layer_started.elapsed().as_millis() as u64;
                        let _ = std::fs::remove_file(&layer.path);
                    }
                    // Drain-close so a mid-flatten abort unblocks the driver's
                    // bounded send promptly.
                    layer_rx.close();
                    // Join the write pool BEFORE anything consumes the tree.
                    // finish() also stamps dir mtimes/ownership and returns
                    // the entry/byte hints that retired the pack stage's
                    // full-tree walks. On a mid-layer abort, finish()'s
                    // error is the root cause (a worker failure echoes into
                    // the reader as a generic abort).
                    match (layer_result, flattener.finish(&mut meta)) {
                        (_, Err(e)) => Err(e.into()),
                        (Err(e), Ok(_)) => Err(e),
                        (Ok(()), Ok(stats)) => Ok((meta, stats, flatten_busy_ms)),
                    }
                },
            )
        };

        let (pull_join, flatten_join) = tokio::join!(pull_driver, flatten_consumer);
        let flatten_result = flatten_join.map_err(|e| {
            MaterializeError::Io(std::io::Error::other(format!("flatten task: {e}")))
        })?;
        // Error precedence: a pull failure truncates the layer stream,
        // which the flattener sees as a short (but well-formed) apply —
        // the pull error is the root cause and wins.
        let pull_ms = pull_join.map_err(|e| {
            MaterializeError::Io(std::io::Error::other(format!("pull task: {e}")))
        })??;
        let (tree_meta, flatten_stats, flatten_busy_ms) = flatten_result?;
        let pipeline_ms = pipeline_started.elapsed().as_millis() as u64;
        let (pulled, flatten_ms) = (resolved, flatten_busy_ms);
        tracing::info!(
            image = %image_uri,
            layers = layer_count,
            entries = tree_meta.len(),
            pipeline_ms,
            pull_ms,
            flatten_busy_ms,
            entry_count_hint = flatten_stats.entry_count_hint,
            tree_bytes_hint = flatten_stats.tree_bytes_hint,
            "flattened layers into tree (pipelined with pull)"
        );
        if !tree_meta.skipped_xattrs.is_empty() {
            tracing::warn!(
                count = tree_meta.skipped_xattrs.len(),
                first = ?tree_meta.skipped_xattrs.first(),
                "some layer xattrs could not be applied to the tree"
            );
        }
        if !tree_meta.skipped_specials.is_empty() {
            tracing::warn!(
                paths = ?tree_meta.skipped_specials,
                "special files (dev nodes/fifos) skipped — mknod requires root; the guest's \
                 devtmpfs provides /dev at boot"
            );
        }

        // 3. Inject the stage-1 init shim. Ownership + mtime clamping
        // happened AT WRITE TIME inside the flatten (ADR 0088 addendum
        // round 2: the old apply_ownership/clamp_mtimes full-tree walks
        // — 44s + a per-entry lchown pass on a dev-brain profile — are
        // folded into the write path; inject stamps its own file).
        inject::inject_init(&rootfs, &self.init).await?;

        // 4. Deterministic pack. The size input comes from the
        // flatten's byte accounting (the old `recursive_size` walk —
        // ~27s of readdir+lstat on a dev-brain tree — retired);
        // `recommended_size`'s 2x + 256 MiB banding absorbs the
        // rounded-length approximation.
        report(MaterializeStage::Pack, None);
        let dir_size = flatten_stats.tree_bytes_hint;
        let ext4_path = work.join("rootfs.ext4");
        let fs_size = ext4::recommended_size(dir_size);
        tracing::info!(
            image = %image_uri,
            dir_size_bytes = dir_size,
            entry_count_hint = flatten_stats.entry_count_hint,
            ext4_size_bytes = fs_size,
            "packing flattened tree to ext4"
        );
        let pack_started = std::time::Instant::now();
        self.packer.pack(&rootfs, &ext4_path, fs_size).await?;
        let pack_ms = pack_started.elapsed().as_millis() as u64;
        // The tree served its purpose. Deleting 500k+ files cost ~40s
        // ON the critical path — rename it aside (instant) and reap it
        // in the background, joined after the chunk stage so scratch
        // hygiene stays deterministic before return.
        let doomed = work.join(format!("rootfs.doomed-{}", uuid::Uuid::new_v4().simple()));
        let tree_reaper = match tokio::fs::rename(&rootfs, &doomed).await {
            Ok(()) => {
                let doomed = doomed.clone();
                Some(tokio::task::spawn_blocking(move || {
                    let _ = std::fs::remove_dir_all(&doomed);
                }))
            }
            Err(e) => {
                tracing::warn!(error = %e, "rootfs rename-aside failed; deleting inline");
                let _ = tokio::fs::remove_dir_all(&rootfs).await;
                None
            }
        };
        let ext4_size_bytes = tokio::fs::metadata(&ext4_path).await?.len();
        report(
            MaterializeStage::Chunk,
            Some(format!("{ext4_size_bytes} ext4 bytes")),
        );

        // 5. Chunk into the content-addressed store. Identity is
        // content-derived (ADR 0036): a deterministic re-materialize
        // reproduces the SAME ManifestRef, so the enable pipeline can
        // recognize "already captured this exact rootfs" and reuse the
        // base snapshot. An already-present manifest is success.
        let chunk_started = std::time::Instant::now();
        // Window-count frames feed the enable UI's chunk progress bar —
        // one per flushed upload window (~every 512 MiB), so a dev-brain
        // ext4 emits ~dozens, not thousands.
        let chunk_progress = |done: u64, total: u64| {
            if let Some(tx) = &progress {
                let _ = tx.try_send(MaterializeProgress {
                    stage: MaterializeStage::Chunk,
                    detail: Some(format!("{ext4_size_bytes} ext4 bytes")),
                    chunks_done: Some(done),
                    chunks_total: Some(total),
                });
            }
        };
        let (manifest, chunk_stats) = chunk_store
            .chunk_file_into(
                &ext4_path,
                ManifestKind::Disk,
                None,
                None,
                Some(&chunk_progress),
            )
            .await?;
        let chunk_ms = chunk_started.elapsed().as_millis() as u64;
        // Background tree deletion overlapped the chunk stage; join it
        // so the scratch guard's final scrub never races a live reaper.
        if let Some(reaper) = tree_reaper {
            let _ = reaper.await;
        }
        let disk_manifest = manifest.content_ref();
        match chunk_store.get_manifest(disk_manifest).await {
            Ok(_) => {
                tracing::debug!(
                    manifest = %disk_manifest,
                    "content-identical manifest already in store; skipping put"
                );
            }
            Err(_) => chunk_store.put_manifest(disk_manifest, &manifest).await?,
        }

        // One-line stage attribution for the whole materialize — the
        // "where did the hours go" answer for a big-image enable, readable
        // straight off the host log without correlating stage frames.
        tracing::info!(
            image = %image_uri,
            platform = %platform,
            manifest = %disk_manifest,
            chunks = manifest.chunks.len(),
            ext4_size_bytes,
            pull_ms,
            flatten_ms,
            pack_ms,
            chunk_ms,
            chunk_scan_ms = (chunk_stats.scan_seconds * 1000.0) as u64,
            chunk_flush_ms = (chunk_stats.flush_seconds * 1000.0) as u64,
            chunk_bytes_uploaded = chunk_stats.bytes_uploaded,
            "materialized image into chunk store"
        );

        Ok(Materialized {
            disk_manifest,
            oci_defaults: pulled.oci_defaults,
            manifest_digest: pulled.manifest_digest,
            ext4_size_bytes,
        })
        // _guard drops here → scratch subdir scrubbed.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_estimate_is_the_adr_hint() {
        assert_eq!(estimated_peak_scratch_bytes(1000), 2500);
        assert_eq!(estimated_peak_scratch_bytes(0), 0);
        // No overflow on adversarial input.
        assert!(estimated_peak_scratch_bytes(u64::MAX) > 0);
    }
}
