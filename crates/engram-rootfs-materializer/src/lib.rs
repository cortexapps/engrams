//! ADR 0080 §C / ADR 0093 — host-side materialization of a
//! **standard** OCI/Docker image into a bootable engram rootfs.
//!
//! Pipeline (ADR 0093, the streaming pack — no intermediate tree, no
//! image file):
//!
//! 1. [`pull`] — resolve the manifest for a platform (manifest lists /
//!    OCI indexes handled), extract the config blob's `ENV`/`WORKDIR`
//!    as [`OciRuntimeDefaults`], stream layer blobs to scratch. Fails
//!    loud on unknown layer mediaTypes before downloading bytes.
//! 2. [`stream_pack`] declare — whiteout-aware layer semantics
//!    (`.wh.`, opaque dirs, hardlinks, symlinks, setuid, xattrs,
//!    zip-slip rejection) applied to an in-memory namespace, pipelined
//!    with the downloads; the init shim is declared alongside
//!    ([`inject`] renders it).
//! 3. seal — replay survivors into `mkext4`, freeze the deterministic
//!    layout (fixed UUID/hash-seed/epoch), emit every metadata byte.
//! 4. fill + chunk — re-decode layers, stream file bytes to final
//!    offsets; each 16 MiB chunk hashes+PUTs the moment its range
//!    completes ([`engram_chunk_store::region`]). The manifest ref is
//!    **content-derived** ([`Manifest::content_ref`]), so
//!    re-materializing unchanged content reproduces the same ref and
//!    base-snapshot reuse keeps working (ADR 0036).
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
pub mod stream_pack;

use std::path::{Path, PathBuf};

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
/// the image's compressed size. ADR 0093: the streaming pack holds
/// only the COMPRESSED layers on scratch (retained through the fill
/// pass) — the tree and the image file no longer exist — so the
/// budget collapses from ~2.5× to ~1.25× (framing + in-flight
/// download temp). An estimate for the disk-veto/budget integration,
/// not a guarantee.
pub fn estimated_peak_scratch_bytes(compressed_image_bytes: u64) -> u64 {
    // ADR 0093: scratch holds the COMPRESSED layers only (kept through
    // the fill pass) — no tree, no image file. 1.25x for tar framing +
    // in-flight download temp.
    compressed_image_bytes.saturating_add(compressed_image_bytes / 4)
}

/// Decompress a pulled layer on its own thread (ChannelReader), so
/// inflate overlaps the consuming pass's work. Shared by the declare
/// and fill passes.
fn decode_layer(
    path: &std::path::Path,
    compression: LayerCompression,
) -> Result<flatten::ChannelReader, std::io::Error> {
    let reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let decoder: Box<dyn std::io::Read + Send> = match compression {
        LayerCompression::Gzip => Box::new(flate2::read::GzDecoder::new(reader)),
        // C-backed zstd (ADR 0088 addendum): ruzstd's single-threaded
        // decode was the historical flatten bottleneck for
        // zstd-layered images.
        LayerCompression::Zstd => Box::new(
            zstd::stream::read::Decoder::with_buffer(reader).map_err(std::io::Error::other)?,
        ),
        LayerCompression::None => Box::new(reader),
    };
    flatten::ChannelReader::spawn(decoder)
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
/// [`engram_oci::RegistryAuthResolver`]) + the init injection to
/// stamp into every produced rootfs. ADR 0093: the ext4 packer seam
/// is gone — packing is the in-process streaming replay.
pub struct Materializer {
    oci: engram_oci::OciClient,
    init: InitInjection,
}

impl Materializer {
    pub fn new(oci: engram_oci::OciClient, init: InitInjection) -> Self {
        Self { oci, init }
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

        let declare_consumer = {
            let progress = progress.clone();
            tokio::task::spawn_blocking(
                move || -> Result<(stream_pack::NamespaceBuilder, Vec<PulledLayer>, u64), MaterializeError> {
                    let mut ns = stream_pack::NamespaceBuilder::new();
                    let mut kept: Vec<PulledLayer> = Vec::new();
                    let mut layer_result: Result<(), MaterializeError> = Ok(());
                    let mut declare_busy_ms = 0u64;
                    while let Some(layer) = layer_rx.blocking_recv() {
                        let applied = kept.len() + 1;
                        // Frame per layer (keepalive-coalesced coord-side;
                        // never re-emits Pull after the first Flatten). The
                        // wire stage stays `Flatten` (4-stage vocabulary —
                        // mid-roll skew drops unknown stages).
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
                        let layer_started = std::time::Instant::now();
                        let piped = match decode_layer(&layer.path, layer.compression) {
                            Ok(p) => p,
                            Err(e) => {
                                layer_result = Err(e.into());
                                break;
                            }
                        };
                        // Headers only — bodies are skipped; the layer FILE
                        // stays on scratch for the fill pass.
                        if let Err(e) = ns.declare_layer(piped) {
                            layer_result = Err(e.into());
                            break;
                        }
                        declare_busy_ms += layer_started.elapsed().as_millis() as u64;
                        kept.push(layer);
                    }
                    // Drain-close so a mid-declare abort unblocks the
                    // driver's bounded send promptly.
                    layer_rx.close();
                    layer_result.map(|()| (ns, kept, declare_busy_ms))
                },
            )
        };

        let (pull_join, declare_join) = tokio::join!(pull_driver, declare_consumer);
        let declare_result = declare_join.map_err(|e| {
            MaterializeError::Io(std::io::Error::other(format!("declare task: {e}")))
        })?;
        // Error precedence: a pull failure truncates the layer stream,
        // which the declare pass sees as a short (but well-formed) tar —
        // the pull error is the root cause and wins.
        let pull_ms = pull_join.map_err(|e| {
            MaterializeError::Io(std::io::Error::other(format!("pull task: {e}")))
        })??;
        let (mut ns, layers, declare_ms) = declare_result?;
        let pipeline_ms = pipeline_started.elapsed().as_millis() as u64;

        // 3. The stage-1 init shim: declared like any other file (no
        // tree exists to write it into); filled from memory in pass 2.
        let (shim_rel, shim_mode, shim_body) = inject::rendered_init_shim(&self.init).await?;
        ns.declare_synthetic(&shim_rel, shim_mode, shim_body)?;

        // 4. Seal: replay survivors into mkext4, freeze the layout.
        // Reported as `Pack` on the wire (4-stage vocabulary).
        report(MaterializeStage::Pack, None);
        let seal_started = std::time::Instant::now();
        let sealed = tokio::task::spawn_blocking(move || ns.seal())
            .await
            .map_err(|e| {
                MaterializeError::Io(std::io::Error::other(format!("seal task: {e}")))
            })??;
        let seal_ms = seal_started.elapsed().as_millis() as u64;
        let image_len = sealed.image_len();
        if !sealed.skipped_xattrs.is_empty() {
            tracing::warn!(
                count = sealed.skipped_xattrs.len(),
                first = ?sealed.skipped_xattrs.first(),
                "some layer xattrs could not be carried into the image"
            );
        }
        tracing::info!(
            image = %image_uri,
            layers = layer_count,
            entries = sealed.entry_count,
            pipeline_ms,
            pull_ms,
            declare_ms,
            seal_ms,
            ext4_size_bytes = image_len,
            "namespace declared + layout sealed (pipelined with pull)"
        );

        // 5. Fill + fused chunk upload (ADR 0093): metadata bytes and
        // zero runs stream out of `begin()` immediately; data follows
        // at ascending offsets as layers re-decode. Every 16 MiB chunk
        // hashes+PUTs the moment its range completes — there is no
        // image file and no separate chunk stage. Identity stays
        // content-derived (ADR 0036): a deterministic re-materialize
        // reproduces the SAME ManifestRef.
        report(
            MaterializeStage::Chunk,
            Some(format!("{image_len} ext4 bytes")),
        );
        let chunk_started = std::time::Instant::now();
        let chunk_progress: Option<std::sync::Arc<dyn Fn(u64, u64) + Send + Sync>> =
            progress.clone().map(|tx| {
                let detail = format!("{image_len} ext4 bytes");
                std::sync::Arc::new(move |done: u64, total: u64| {
                    let _ = tx.try_send(MaterializeProgress {
                        stage: MaterializeStage::Chunk,
                        detail: Some(detail.clone()),
                        chunks_done: Some(done),
                        chunks_total: Some(total),
                    });
                }) as std::sync::Arc<dyn Fn(u64, u64) + Send + Sync>
            });
        let (mut rc, uploader) =
            chunk_store.region_chunker(ManifestKind::Disk, image_len, chunk_progress);
        let fill_task =
            tokio::task::spawn_blocking(move || -> Result<(u64, u64), MaterializeError> {
                let mut sink = stream_pack::ChunkerSink(&mut rc);
                let mut w = sealed.begin(&mut sink)?;
                let mut fill_busy_ms = 0u64;
                for (i, layer) in layers.iter().enumerate() {
                    if sealed.layer_fill_count(i) > 0 {
                        let piped = decode_layer(&layer.path, layer.compression)?;
                        let started = std::time::Instant::now();
                        sealed.fill_layer(&mut w, i, piped)?;
                        fill_busy_ms += started.elapsed().as_millis() as u64;
                    }
                    let _ = std::fs::remove_file(&layer.path);
                }
                sealed.fill_synthetic(&mut w)?;
                stream_pack::SealedImage::finish_writer(w)?;
                let high_water = rc.finish()?;
                Ok((fill_busy_ms, high_water))
            });
        let fill_join = fill_task.await;
        let upload_join = uploader.await;
        // The uploader's error is the root cause when both fail (a PUT
        // failure closes the channel, which the fill pass surfaces as a
        // generic "uploader terminated").
        let (manifest, chunk_stats) = upload_join
            .map_err(|e| MaterializeError::Io(std::io::Error::other(format!("uploader: {e}"))))??;
        let (fill_ms, partial_high_water) = fill_join.map_err(|e| {
            MaterializeError::Io(std::io::Error::other(format!("fill task: {e}")))
        })??;
        let chunk_ms = chunk_started.elapsed().as_millis() as u64;
        let ext4_size_bytes = image_len;

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
            declare_ms,
            seal_ms,
            fill_ms,
            chunk_ms,
            chunks_uploaded = chunk_stats.chunks_uploaded,
            chunks_elided = chunk_stats.chunks_elided,
            chunk_bytes_uploaded = chunk_stats.bytes_uploaded,
            partial_high_water,
            "materialized image into chunk store (streaming pack, no image file)"
        );

        Ok(Materialized {
            disk_manifest,
            oci_defaults: resolved.oci_defaults,
            manifest_digest: resolved.manifest_digest,
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
        // ADR 0093: compressed layers only — no tree, no image file.
        assert_eq!(estimated_peak_scratch_bytes(1000), 1250);
        assert_eq!(estimated_peak_scratch_bytes(0), 0);
        // No overflow on adversarial input.
        assert!(estimated_peak_scratch_bytes(u64::MAX) > 0);
    }
}
