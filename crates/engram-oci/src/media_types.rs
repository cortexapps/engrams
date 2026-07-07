//! Custom OCI mediaType constants for Engram artifacts.
//!
//! Standard registries (GHCR/GCR/ECR/Harbor/registry:2) accept
//! arbitrary mediaTypes since the OCI Distribution Spec was updated
//! in 2022. We use the `application/vnd.engram.*` namespace so an
//! external observer can recognize Engram artifacts without ambiguity.

/// Bake image config (small JSON: format, repo/tag, and — ADR 0080 —
/// `runtime_defaults`, the Dockerfile ENV/WORKDIR the enable pipeline
/// persists onto the enabled_images row).
pub const ENGRAM_IMAGE_CONFIG_MEDIA_TYPE: &str = "application/vnd.engram.image.v1+json";

// ADR 0080 retired `ENGRAM_MANIFEST_MEDIA_TYPE` (the manifest.toml
// layer): the bake carries no runtime config anymore — it arrives
// out-of-band via the ImageService (`image enable --config`).

/// Bake image rootfs layer — raw `rootfs.ext4` bytes (large blob).
pub const ENGRAM_ROOTFS_EXT4_MEDIA_TYPE: &str = "application/vnd.engram.rootfs.ext4.v1";

/// ADR 0007 bake bundle — tiny JSON pointing at content-addressed
/// chunk manifests in BlobStorage. Production pull paths resolve
/// disk + memory content through the chunk store via these refs;
/// the rootfs.ext4 layer above stays alongside until Phase 6 wires
/// SandboxBackend to take `ManifestRef` natively.
pub const ENGRAM_BUNDLE_MEDIA_TYPE: &str = "application/vnd.engram.bundle.v1+json";

/// Harness pack config (currently unused beyond the mediaType marker).
pub const ENGRAM_HARNESS_CONFIG_MEDIA_TYPE: &str = "application/vnd.engram.harness.v1+json";

/// Harness pack layer — gzip'd tar of the pack directory.
pub const ENGRAM_HARNESS_TAR_MEDIA_TYPE: &str = "application/vnd.engram.harness.tar.v1+gzip";

// ---- ADR 0008 Phase 3 / ADR 0036: chunked image layers ----
//
// A chunked image artifact carries a small bootstrap JSON index
// plus one OCI blob **per chunk** (ADR 0036). Each chunk layer's
// OCI digest is `sha256:<chunk-hash>` — the same sha256 the chunk
// is addressed by in BlobStorage and in every `ChunkResolver` — so
// the registry's content addressing and ours coincide. Push skips
// blobs the registry already has (delta upload); pull fetches each
// missing chunk as a plain blob GET (delta download).
//
// ADR 0036 retired the monolithic `chunks.disk.v1` layer (one
// concatenated multi-GB blob): a single upload session for ~10 GB
// failed un-resumably, sat at GHCR's 10 GB layer ceiling, and made
// cross-bake dedup impossible at the registry.

/// Bootstrap layer for the disk side. JSON content; see
/// `engram_chunk_store::Bootstrap`.
pub const ENGRAM_BOOTSTRAP_DISK_MEDIA_TYPE: &str = "application/vnd.engram.bootstrap.disk.v1+json";

/// A single chunk of a chunked image (ADR 0036). Layer digest is
/// `sha256:<chunk-hash>`; nominal size 16 MiB for disk chunks (the
/// final chunk of an image may be shorter).
pub const ENGRAM_CHUNK_MEDIA_TYPE: &str = "application/vnd.engram.chunk.v1";
