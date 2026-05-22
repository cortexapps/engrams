//! Custom OCI mediaType constants for Engram artifacts.
//!
//! Standard registries (GHCR/GCR/ECR/Harbor/registry:2) accept
//! arbitrary mediaTypes since the OCI Distribution Spec was updated
//! in 2022. We use the `application/vnd.engram.*` namespace so an
//! external observer can recognize Engram artifacts without ambiguity.

/// Bake image config (small JSON: format, agent_version, transport).
pub const ENGRAM_IMAGE_CONFIG_MEDIA_TYPE: &str = "application/vnd.engram.image.v1+json";

/// Bake image manifest layer — the existing `manifest.toml` content.
pub const ENGRAM_MANIFEST_MEDIA_TYPE: &str = "application/vnd.engram.manifest.v1+toml";

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

// ---- ADR 0008 Phase 3: Nydus-shaped chunked image layers ----
//
// A chunked image artifact carries (bootstrap, chunk_blob) per
// kind (disk, memory). The bootstrap is a small JSON index from
// `ChunkHash` to `(blob_offset, length)`; the chunk blob is the
// concatenation of all chunks in bootstrap-entry order, pulled
// lazily via Range GET at fault time.

/// Bootstrap layer for the disk side. JSON content; see
/// `engram_chunk_store::Bootstrap`.
pub const ENGRAM_BOOTSTRAP_DISK_MEDIA_TYPE: &str = "application/vnd.engram.bootstrap.disk.v1+json";

/// Chunk-blob layer for the disk side. Opaque concatenation of
/// 16 MiB chunks; consumers do Range GET against the OCI registry's
/// `/v2/<repo>/blobs/<digest>` endpoint to pull individual chunks.
pub const ENGRAM_CHUNKS_DISK_MEDIA_TYPE: &str = "application/vnd.engram.chunks.disk.v1";
