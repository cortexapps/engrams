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

/// Harness pack config (currently unused beyond the mediaType marker).
pub const ENGRAM_HARNESS_CONFIG_MEDIA_TYPE: &str = "application/vnd.engram.harness.v1+json";

/// Harness pack layer — gzip'd tar of the pack directory.
pub const ENGRAM_HARNESS_TAR_MEDIA_TYPE: &str = "application/vnd.engram.harness.tar.v1+gzip";
