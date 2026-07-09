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
//!    (`.wh.`, opaque dirs, hardlinks, symlinks, setuid, metadata
//!    sidecar, zip-slip rejection).
//! 3. [`inject`] — write the stage-1 init shim (the ONE engrams file
//!    in the rootfs).
//! 4. [`ext4`] — deterministic tar emit plus mke2fs pack (fixed
//!    UUID/hash-seed + `SOURCE_DATE_EPOCH`).
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

pub use ext4::{emit_tar, recommended_size, recursive_size, Ext4Error, Ext4Packer, Mke2fsPacker};
pub use flatten::{
    apply_layer, EntryMeta, FlattenError, SkippedSpecial, SkippedSpecialKind, TreeMetadata,
};
pub use inject::{inject_init, InitInjection, Transport, DEFAULT_INIT_SHIM};
pub use pull::{
    layer_compression, pull_image, LayerCompression, Platform, PullError, PulledImage, PulledLayer,
};

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

#[derive(Debug, Eq, PartialEq)]
struct DroppedXattrs {
    affected_entries: usize,
    examples: Vec<String>,
}

fn mke2fs_accepts_tar_xattr(name: &str) -> bool {
    matches!(name, "security.capability" | "gnu.translator")
}

fn mke2fs_dropped_xattrs(tree_meta: &TreeMetadata) -> DroppedXattrs {
    let mut affected_entries = 0;
    let mut examples = Vec::new();

    for (path, meta) in tree_meta.iter() {
        let mut entry_has_dropped_xattr = false;
        for xattr_name in meta.xattrs.keys() {
            if mke2fs_accepts_tar_xattr(xattr_name) {
                continue;
            }
            entry_has_dropped_xattr = true;
            if examples.len() < 3 {
                examples.push(format!("{path}:{xattr_name}"));
            }
        }
        if entry_has_dropped_xattr {
            affected_entries += 1;
        }
    }

    DroppedXattrs {
        affected_entries,
        examples,
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
    /// one per pipeline stage transition — the `MaterializeImage` RPC
    /// forwards them coord-ward; the keepalive cadence (≤30 s) is the
    /// CALLER's job (it re-sends the last frame), this crate only
    /// signals honest transitions. `None` = silent (tests, the bake).
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
                let _ = tx.try_send(MaterializeProgress { stage, detail });
            }
        };

        tokio::fs::create_dir_all(scratch_dir).await?;
        let work = scratch_dir.join(format!("materialize-{}", uuid::Uuid::new_v4().simple()));
        let _guard = ScratchGuard { dir: work.clone() };

        // 1. Pull.
        report(MaterializeStage::Pull, Some(format!("{platform}")));
        let layers_dir = work.join("layers");
        let pulled = pull::pull_image(&self.oci, image_uri, platform, &layers_dir).await?;

        // 2. Flatten (sync tar/decompress IO — off the async runtime).
        // Each layer file is deleted as soon as it's applied, so the
        // scratch peak during the flatten is tree + ONE layer.
        report(
            MaterializeStage::Flatten,
            Some(format!(
                "{} layers, {} compressed bytes",
                pulled.layers.len(),
                pulled.compressed_bytes
            )),
        );
        let rootfs = work.join("rootfs");
        tokio::fs::create_dir_all(&rootfs).await?;
        let tree_meta = {
            let rootfs = rootfs.clone();
            let layers = pulled.layers.clone();
            tokio::task::spawn_blocking(move || -> Result<TreeMetadata, MaterializeError> {
                let mut meta = TreeMetadata::default();
                for layer in &layers {
                    let file = std::fs::File::open(&layer.path)?;
                    let reader = std::io::BufReader::new(file);
                    match layer.compression {
                        LayerCompression::Gzip => flatten::apply_layer(
                            &rootfs,
                            &mut meta,
                            flate2::read::GzDecoder::new(reader),
                        )?,
                        LayerCompression::Zstd => flatten::apply_layer(
                            &rootfs,
                            &mut meta,
                            ruzstd::decoding::StreamingDecoder::new(reader)
                                .map_err(|e| std::io::Error::other(e.to_string()))?,
                        )?,
                        LayerCompression::None => flatten::apply_layer(&rootfs, &mut meta, reader)?,
                    }
                    let _ = std::fs::remove_file(&layer.path);
                }
                Ok(meta)
            })
            .await
            .map_err(|e| {
                MaterializeError::Io(std::io::Error::other(format!("flatten task: {e}")))
            })??
        };
        if !tree_meta.skipped_specials.is_empty() {
            tracing::warn!(
                paths = ?tree_meta.skipped_specials,
                "special files recorded in sidecar for tar emit"
            );
        }
        let dropped_xattrs = mke2fs_dropped_xattrs(&tree_meta);
        if dropped_xattrs.affected_entries != 0 {
            tracing::warn!(
                affected_entries = dropped_xattrs.affected_entries,
                examples = ?dropped_xattrs.examples,
                "mke2fs tar input will drop non-whitelisted xattrs"
            );
        }

        // 3. Inject the stage-1 init shim.
        inject::inject_init(&rootfs, &self.init).await?;

        // 4. Deterministic pack: emit metadata-authoritative tar, delete
        // the host-content tree, then mke2fs -d <tar>.
        report(MaterializeStage::Pack, None);
        let dir_size = ext4::recursive_size(&rootfs).await?;
        let ext4_path = work.join("rootfs.ext4");
        let tar_path = work.join("rootfs.tar");
        let fs_size = ext4::recommended_size(dir_size);
        tracing::info!(
            image = %image_uri,
            dir_size_bytes = dir_size,
            ext4_size_bytes = fs_size,
            "emitting flattened tree tar and packing to ext4"
        );
        {
            let rootfs_c = rootfs.clone();
            let tar_path_c = tar_path.clone();
            tokio::task::spawn_blocking(move || ext4::emit_tar(&rootfs_c, &tree_meta, &tar_path_c))
                .await
                .map_err(|e| {
                    MaterializeError::Io(std::io::Error::other(format!("tar emit task: {e}")))
                })??;
        }
        // The tree served its purpose — free it before mke2fs so the
        // scratch peak is tree+tar, then tar+ext4.
        let _ = tokio::fs::remove_dir_all(&rootfs).await;
        self.packer.pack(&tar_path, &ext4_path, fs_size).await?;
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
        let manifest = chunk_store
            .chunk_file(&ext4_path, ManifestKind::Disk, None)
            .await?;
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

        tracing::info!(
            image = %image_uri,
            platform = %platform,
            manifest = %disk_manifest,
            chunks = manifest.chunks.len(),
            ext4_size_bytes,
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

    #[test]
    fn mke2fs_dropped_xattrs_reports_only_non_whitelisted_and_truncates_examples() {
        fn entry_with_xattrs(names: &[&str]) -> EntryMeta {
            let mut entry = EntryMeta::new(0, 0, 0o644);
            for name in names {
                entry.xattrs.insert((*name).to_string(), Vec::new());
            }
            entry
        }

        let mut meta = TreeMetadata::default();
        meta.insert(
            "bin/ok".to_string(),
            entry_with_xattrs(&["gnu.translator", "security.capability"]),
        );
        assert_eq!(
            mke2fs_dropped_xattrs(&meta),
            DroppedXattrs {
                affected_entries: 0,
                examples: Vec::new(),
            },
        );

        meta.insert(
            "bin/mixed".to_string(),
            entry_with_xattrs(&["security.capability", "security.selinux", "user.mime_type"]),
        );
        meta.insert(
            "etc/a".to_string(),
            entry_with_xattrs(&["trusted.overlay.opaque"]),
        );
        meta.insert("etc/b".to_string(), entry_with_xattrs(&["user.comment"]));
        meta.insert("etc/c".to_string(), entry_with_xattrs(&["user.extra"]));

        let dropped = mke2fs_dropped_xattrs(&meta);
        assert_eq!(dropped.affected_entries, 4);
        assert_eq!(
            dropped.examples,
            vec![
                "bin/mixed:security.selinux",
                "bin/mixed:user.mime_type",
                "etc/a:trusted.overlay.opaque",
            ],
        );
    }
}
