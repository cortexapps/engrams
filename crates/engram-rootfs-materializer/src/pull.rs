//! Pull stage (ADR 0080 §C): resolve a standard OCI/Docker image for
//! one platform, extract the config blob's runtime defaults, and
//! stream each layer blob to scratch.
//!
//! The registry mechanics (auth, index → platform-manifest resolution,
//! digest-verified blob streaming) live in `engram-oci`
//! (`docker_image.rs`) — this module owns the engrams-side policy:
//! **fail loud on unknown layer mediaTypes** (validated for every
//! layer BEFORE any layer bytes are downloaded) and the config-blob →
//! [`OciRuntimeDefaults`] extraction.

use std::path::{Path, PathBuf};

use engram_core::types::image::OciRuntimeDefaults;
use engram_oci::OciClient;
use serde::Deserialize;

/// Guest platform to materialize for. An explicit parameter (never
/// inferred inside this crate): the phase-3b host RPC passes the
/// host's own arch, and tests pin both values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    LinuxAmd64,
    LinuxArm64,
}

impl Platform {
    pub fn os(self) -> &'static str {
        "linux"
    }

    /// GOARCH-style architecture string, as OCI platform entries use.
    pub fn architecture(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "amd64",
            Self::LinuxArm64 => "arm64",
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.os(), self.architecture())
    }
}

/// How a layer blob is compressed on the wire. Derived strictly from
/// the manifest's mediaType — anything unrecognized is a hard error
/// naming the type (the ADR's "fail loud on unknown media types").
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayerCompression {
    /// Plain uncompressed tar.
    None,
    Gzip,
    Zstd,
}

/// Map an OCI/Docker layer mediaType to its compression. The accepted
/// set is deliberately exact — a typo'd or exotic type must fail the
/// materialize, never decode garbage.
pub fn layer_compression(media_type: &str) -> Result<LayerCompression, PullError> {
    match media_type {
        "application/vnd.docker.image.rootfs.diff.tar.gzip"
        | "application/vnd.oci.image.layer.v1.tar+gzip" => Ok(LayerCompression::Gzip),
        "application/vnd.oci.image.layer.v1.tar+zstd" => Ok(LayerCompression::Zstd),
        "application/vnd.docker.image.rootfs.diff.tar"
        | "application/vnd.oci.image.layer.v1.tar" => Ok(LayerCompression::None),
        other => Err(PullError::UnsupportedLayerMediaType(other.to_string())),
    }
}

/// One layer landed on scratch, ready for the flatten stage.
#[derive(Clone, Debug)]
pub struct PulledLayer {
    pub path: PathBuf,
    pub compression: LayerCompression,
    pub digest: String,
}

/// Output of the pull stage.
#[derive(Debug)]
pub struct PulledImage {
    /// Layers in application order (base first).
    pub layers: Vec<PulledLayer>,
    /// Dockerfile `ENV` + `WORKDIR` from the image config blob.
    pub oci_defaults: OciRuntimeDefaults,
    /// Sum of the layers' compressed sizes (the scratch-budget input).
    pub compressed_bytes: u64,
    /// Digest (`sha256:<hex>`) of the PLATFORM-resolved image manifest
    /// (not the index) — what the enable pipeline stamps as the row's
    /// `manifest_digest` (ADR 0080 phase 3b).
    pub manifest_digest: String,
}

#[derive(Debug)]
pub enum PullError {
    Oci(engram_oci::OciError),
    /// A layer's mediaType is not in the supported set. Carries the
    /// offending type verbatim.
    UnsupportedLayerMediaType(String),
    /// The image config blob didn't parse as an OCI image config.
    Config(String),
    Io(std::io::Error),
}

impl std::fmt::Display for PullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Oci(e) => write!(f, "registry: {e}"),
            Self::UnsupportedLayerMediaType(t) => write!(
                f,
                "unsupported layer mediaType {t:?} — expected a docker/OCI tar layer \
                 (gzip, zstd, or uncompressed)"
            ),
            Self::Config(m) => write!(f, "image config blob: {m}"),
            Self::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for PullError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Oci(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<engram_oci::OciError> for PullError {
    fn from(e: engram_oci::OciError) -> Self {
        Self::Oci(e)
    }
}

impl From<std::io::Error> for PullError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// The half of a standard OCI image config blob we consume: the
/// runtime `config` section's `Env` + `WorkingDir` (Go-style key
/// casing per the image spec).
#[derive(Debug, Default, Deserialize)]
struct ImageConfigBlob {
    #[serde(default)]
    config: Option<ImageConfigSection>,
}

#[derive(Debug, Default, Deserialize)]
struct ImageConfigSection {
    #[serde(rename = "Env", default)]
    env: Vec<String>,
    #[serde(rename = "WorkingDir", default)]
    working_dir: Option<String>,
}

/// A platform-resolved image: everything known BEFORE any layer bytes
/// move. mediaTypes are validated here (fail loud), so a download plan
/// can never contain an unflattenable layer.
#[derive(Debug)]
pub struct ResolvedImage {
    pub manifest_digest: String,
    pub oci_defaults: OciRuntimeDefaults,
    /// Layers in application order (base first).
    pub layers: Vec<LayerPlan>,
    /// Sum of the layers' compressed sizes (the scratch-budget input).
    pub compressed_bytes: u64,
}

/// One planned (not yet downloaded) layer.
#[derive(Clone, Debug)]
pub struct LayerPlan {
    pub blob: engram_oci::DockerBlobRef,
    pub compression: LayerCompression,
    /// Manifest position — application order AND the scratch filename.
    pub index: usize,
}

/// Resolve the platform manifest, validate every layer's mediaType
/// (before downloading any bytes), and fetch the config blob →
/// [`OciRuntimeDefaults`]. The download side is [`download_layer`];
/// the materializer pipelines the two (ADR 0088 addendum).
pub async fn resolve_image(
    oci: &OciClient,
    image_uri: &str,
    platform: Platform,
) -> Result<ResolvedImage, PullError> {
    let manifest = oci
        .pull_docker_manifest(image_uri, platform.os(), platform.architecture())
        .await?;

    // Fail loud on ANY unknown layer mediaType before pulling bytes —
    // a partially-downloaded image that can't flatten is pure waste.
    let compressions = manifest
        .layers
        .iter()
        .map(|l| layer_compression(&l.media_type))
        .collect::<Result<Vec<_>, _>>()?;

    let config_bytes = oci
        .pull_docker_blob_to_vec(image_uri, &manifest.config)
        .await?;
    let parsed: ImageConfigBlob = serde_json::from_slice(&config_bytes)
        .map_err(|e| PullError::Config(format!("parse {}: {e}", manifest.config.digest)))?;
    let section = parsed.config.unwrap_or_default();
    let oci_defaults =
        OciRuntimeDefaults::from_docker_config(&section.env, section.working_dir.as_deref());

    let mut compressed_bytes = 0u64;
    let layers = manifest
        .layers
        .iter()
        .zip(compressions)
        .enumerate()
        .map(|(index, (blob, compression))| {
            compressed_bytes = compressed_bytes.saturating_add(blob.size);
            LayerPlan {
                blob: blob.clone(),
                compression,
                index,
            }
        })
        .collect();

    Ok(ResolvedImage {
        manifest_digest: manifest.manifest_digest.as_str().to_string(),
        oci_defaults,
        layers,
        compressed_bytes,
    })
}

/// Stream one planned layer to `layers_dir/<index>.layer`. Digest
/// verification + range-resume retry live in `engram-oci`.
pub async fn download_layer(
    oci: &OciClient,
    image_uri: &str,
    plan: &LayerPlan,
    layers_dir: &Path,
) -> Result<PulledLayer, PullError> {
    let path = layers_dir.join(format!("{:04}.layer", plan.index));
    oci.pull_docker_blob_to_file(image_uri, &plan.blob, &path)
        .await?;
    tracing::debug!(
        layer = plan.index,
        digest = %plan.blob.digest,
        bytes = plan.blob.size,
        compression = ?plan.compression,
        "layer pulled to scratch"
    );
    Ok(PulledLayer {
        path,
        compression: plan.compression,
        digest: plan.blob.digest.clone(),
    })
}

/// Run the whole pull stage sequentially: [`resolve_image`] + one
/// [`download_layer`] per layer. The single-caller convenience (tests,
/// the bake); the materializer's hot path pipelines downloads with the
/// flatten instead.
pub async fn pull_image(
    oci: &OciClient,
    image_uri: &str,
    platform: Platform,
    layers_dir: &Path,
) -> Result<PulledImage, PullError> {
    let resolved = resolve_image(oci, image_uri, platform).await?;
    tokio::fs::create_dir_all(layers_dir).await?;
    let mut layers = Vec::with_capacity(resolved.layers.len());
    for plan in &resolved.layers {
        layers.push(download_layer(oci, image_uri, plan, layers_dir).await?);
    }

    tracing::info!(
        image = %image_uri,
        platform = %platform,
        layers = layers.len(),
        compressed_bytes = resolved.compressed_bytes,
        manifest = %resolved.manifest_digest,
        "image pulled"
    );

    Ok(PulledImage {
        layers,
        oci_defaults: resolved.oci_defaults,
        compressed_bytes: resolved.compressed_bytes,
        manifest_digest: resolved.manifest_digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The supported mediaType set, exactly — and the fail-loud path
    /// carries the offending type's name (the ADR risk item).
    #[test]
    fn layer_compression_supported_set_and_fail_loud() {
        assert_eq!(
            layer_compression("application/vnd.docker.image.rootfs.diff.tar.gzip").unwrap(),
            LayerCompression::Gzip
        );
        assert_eq!(
            layer_compression("application/vnd.oci.image.layer.v1.tar+gzip").unwrap(),
            LayerCompression::Gzip
        );
        assert_eq!(
            layer_compression("application/vnd.oci.image.layer.v1.tar+zstd").unwrap(),
            LayerCompression::Zstd
        );
        assert_eq!(
            layer_compression("application/vnd.oci.image.layer.v1.tar").unwrap(),
            LayerCompression::None
        );
        assert_eq!(
            layer_compression("application/vnd.docker.image.rootfs.diff.tar").unwrap(),
            LayerCompression::None
        );

        for unknown in [
            "application/vnd.oci.image.layer.v1.tar+bzip2",
            "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip",
            "application/octet-stream",
        ] {
            let err = layer_compression(unknown).expect_err("must fail loud");
            assert!(
                err.to_string().contains(unknown),
                "error must name the mediaType: {err}"
            );
        }
    }

    #[test]
    fn platform_strings() {
        assert_eq!(Platform::LinuxAmd64.to_string(), "linux/amd64");
        assert_eq!(Platform::LinuxArm64.to_string(), "linux/arm64");
    }
}
