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

/// Bootstrap layer for the canonical memory side. Same shape as
/// the disk bootstrap; entries are 512 KB chunks of a memory.bin.
/// Optional in the artifact — only present on bakes that ran
/// canonical-memory capture.
pub const ENGRAM_BOOTSTRAP_MEMORY_MEDIA_TYPE: &str =
    "application/vnd.engram.bootstrap.memory.v1+json";

/// Chunk-blob layer for the canonical memory side. Concatenated
/// 512 KB chunks. Pulled via Range GET on UFFD fault when the
/// chunk isn't in the local NVMe cache and isn't in BlobStorage.
pub const ENGRAM_CHUNKS_MEMORY_MEDIA_TYPE: &str = "application/vnd.engram.chunks.memory.v1";

// ---- ADR 0014 M1.3 / M1.11: canonical-snapshot state + sidecar ----
//
// The bake's `capture_canonical_memory` produces a portable FC
// snapshot. Three pieces are needed to restore on a sibling host:
//
//   1. memory.bin — chunked into the (memory bootstrap, chunks
//      memory) layers above. Large.
//   2. state.bin — opaque FC vCPU/device state. Small (few MiB).
//   3. manifest.json — the FC sidecar describing the snapshot
//      (memory layout, drives, etc.). Tiny.
//
// Before these media types existed, the bake uploaded #2 and #3
// directly to `BlobStorage` at canonical keys derived from
// snapshot_id. That coupled the bake's environment to the
// production deployment's blob backend — a bake on a CI runner
// with no GCS credentials produced an OCI artifact prod hosts
// couldn't restore. With these layers in OCI, the bake is
// self-contained: `engram-coordinator::enable_image` pulls them
// and writes to its own `BlobStorage` at the canonical keys.

/// Engram canonical-snapshot `state.bin` layer — opaque FC state
/// bytes captured at bake time. Small (a few MiB). Coord
/// materializes to BlobStorage at `state_blob_key(snapshot_id)`
/// on enable-image so host-agents can restore without needing
/// to re-fetch from OCI.
pub const ENGRAM_SNAPSHOT_STATE_MEDIA_TYPE: &str = "application/vnd.engram.snapshot.state.v1";

/// Engram canonical-snapshot sidecar `manifest.json` layer — FC's
/// own JSON manifest describing the snapshot's memory regions +
/// device layout. Tiny. Coord materializes to BlobStorage at
/// `sidecar_blob_key(snapshot_id)` on enable-image.
pub const ENGRAM_SNAPSHOT_SIDECAR_MEDIA_TYPE: &str =
    "application/vnd.engram.snapshot.sidecar.v1+json";

/// ADR 0014 M1.14: bake-time working-set trace. JSON encoding of the
/// `WorkingSetTrace` recorded by a synthetic profiling pass: bake
/// restores the just-taken snapshot, exercises mount(2)+execve(2)
/// on the stub harness, lets the UFFD recorder accumulate ~3s of
/// chunk faults, then dumps. Tiny (~1 KiB). Coord materializes to
/// BlobStorage at `working_set_blob_key(snapshot_id)` so warm-pool
/// refill can narrow M1.13's parallel prefetch to just the working
/// set. Absent layer = no canonical trace; refill falls back to
/// full-manifest prefetch.
pub const ENGRAM_SNAPSHOT_WORKING_SET_MEDIA_TYPE: &str =
    "application/vnd.engram.snapshot.working-set.v1+json";
