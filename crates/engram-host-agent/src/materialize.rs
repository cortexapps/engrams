//! ADR 0080 §C — the host half of the `MaterializeImage` RPC.
//!
//! Turns a **standard** docker/OCI image into a chunked bootable ext4
//! via `engram-rootfs-materializer` (pull → flatten → inject stage-1
//! init → mke2fs pack → chunk). The chunks + content-derived manifest
//! land in the host's [`ChunkStore`], which is wired write-through:
//! the durable tier is BlobStorage (`ChunkStore::new(blob)`) with the
//! NVMe chunk cache as the local write-through layer (ADR 0078) — so
//! the coordinator (and every other host) can read the returned
//! manifest from BlobStorage with **no explicit upload step**, exactly
//! like the snapshot path.
//!
//! Host safeguards enforced here (the ADR's list):
//!
//! - **enable-time size cap**: summed compressed layer sizes from the
//!   docker manifest, checked BEFORE any layer bytes are pulled
//!   (`ENGRAM_MATERIALIZE_MAX_IMAGE_BYTES`, default 80 GiB).
//! - **disk headroom**: `estimated_peak_scratch_bytes` (~2.5× the
//!   compressed size) vs `statvfs` free space on the scratch volume —
//!   fail loud with both numbers.
//! - **≤1 concurrent materialize per host**: the `try_lock` gate lives
//!   on `PooledBackend::materialize_image` (the RPC entry); a second
//!   call gets a retryable `MaterializeFailureKind::Busy`.
//! - **scratch scrub**: the materializer's `ScratchGuard` scrubs its
//!   per-run subdir on success/error/panic; [`reconcile_scratch`]
//!   sweeps orphans (a host-agent crash mid-run) at startup.

use std::path::Path;
use std::sync::Arc;

use engram_core::error::SandboxError;
use engram_core::types::registry::ResolvedRegistryAuth;
use engram_core::types::{
    MaterializeFailure, MaterializeFailureKind, MaterializeProgress, MaterializedImage,
};
use engram_oci::{BasicCreds, OciClient, OciError, RegistryAuthResolver};
use engram_rootfs_materializer::{
    estimated_peak_scratch_bytes, InitInjection, MaterializeError, Materializer, Platform,
    PullError, Transport,
};

/// Enable-time image size cap: the summed COMPRESSED layer bytes a
/// single materialize may pull. Big enough for every real dev image
/// (dev-brain's artifact era topped out ~33 GB), small enough that a
/// mis-tagged multi-hundred-GiB image can't wedge a host's disk.
pub const MAX_IMAGE_BYTES_ENV: &str = "ENGRAM_MATERIALIZE_MAX_IMAGE_BYTES";
const DEFAULT_MAX_IMAGE_BYTES: u64 = 80 * 1024 * 1024 * 1024;

fn max_image_bytes() -> u64 {
    match std::env::var(MAX_IMAGE_BYTES_ENV) {
        Ok(raw) => match raw.parse::<u64>() {
            Ok(v) if v > 0 => v,
            _ => {
                tracing::warn!(
                    raw = %raw,
                    default = DEFAULT_MAX_IMAGE_BYTES,
                    "{MAX_IMAGE_BYTES_ENV} must be a positive u64; using the default"
                );
                DEFAULT_MAX_IMAGE_BYTES
            }
        },
        Err(_) => DEFAULT_MAX_IMAGE_BYTES,
    }
}

fn failed(kind: MaterializeFailureKind, message: String) -> SandboxError {
    SandboxError::MaterializeFailed(MaterializeFailure { kind, message })
}

/// Startup reconcile: remove orphaned per-run scratch subdirs
/// (`materialize-<uuid>`) left by a host-agent that died mid-run — the
/// one path the in-process `ScratchGuard` can't cover. Safe to run
/// unconditionally: the ≤1-concurrent gate means nothing under this
/// dir is live at startup (the previous process is gone).
pub fn reconcile_scratch(dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "materialize scratch reconcile: read_dir failed");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                tracing::warn!(
                    path = %path.display(),
                    "removed orphaned materialize scratch (previous host-agent died mid-run)"
                );
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "materialize scratch reconcile: remove failed");
            }
        }
    }
}

/// Per-request resolver serving the coordinator-resolved static creds
/// for every registry host. Sound because the client built from it
/// lives for exactly one materialize of one image — there is only one
/// registry in play.
struct StaticAuthResolver(ResolvedRegistryAuth);

#[async_trait::async_trait]
impl RegistryAuthResolver for StaticAuthResolver {
    async fn resolve(&self, _registry_host: &str) -> Result<Option<BasicCreds>, OciError> {
        Ok(Some(BasicCreds {
            username: self.0.username.clone(),
            password: self.0.password.clone(),
        }))
    }
}

/// Map the OCI platform-arch string this host serves. The coordinator
/// stamps ITS arch on the request (coord + hosts are same-arch per
/// deployment); validating here fails loud if that assumption ever
/// breaks (a mixed-arch fleet needs an arch-aware picker first).
fn host_platform() -> Option<Platform> {
    match std::env::consts::ARCH {
        "x86_64" => Some(Platform::LinuxAmd64),
        "aarch64" => Some(Platform::LinuxArm64),
        _ => None,
    }
}

/// Validate + run one materialize. Called by
/// `PooledBackend::materialize_image` UNDER its ≤1-concurrent gate.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    host_oci: Option<OciClient>,
    chunk_store: &engram_chunk_store::ChunkStore,
    scratch: &Path,
    image_uri: &str,
    platform_os: &str,
    platform_arch: &str,
    registry_auth: Option<ResolvedRegistryAuth>,
    progress: tokio::sync::mpsc::Sender<MaterializeProgress>,
) -> Result<MaterializedImage, SandboxError> {
    // ---- platform validation (fail loud on a mismatch) ----
    let platform = host_platform().ok_or_else(|| {
        SandboxError::InvalidSpec(format!(
            "materialize_image: unsupported host arch {}",
            std::env::consts::ARCH
        ))
    })?;
    if platform_os != platform.os() || platform_arch != platform.architecture() {
        return Err(SandboxError::InvalidSpec(format!(
            "materialize_image: requested platform {platform_os}/{platform_arch} but this \
             host materializes {platform} — coordinator/host arch mismatch"
        )));
    }

    // ---- registry auth ----
    // Some(creds): coordinator-resolved static basic auth for this one
    // pull. None: the host's ambient resolver — the image-cache
    // OciClient whose HttpAuthResolver asks the coordinator (which
    // covers GCP workload identity + static rows uniformly), or plain
    // anonymous when this host has no OCI client wired (dev).
    let oci = match registry_auth {
        Some(creds) => OciClient::new(Arc::new(StaticAuthResolver(creds))),
        None => host_oci.unwrap_or_else(|| OciClient::new(Arc::new(engram_oci::AnonymousResolver))),
    };

    // ---- enable-time size cap, BEFORE pulling any layer bytes ----
    let manifest = oci
        .pull_docker_manifest(image_uri, platform_os, platform_arch)
        .await
        .map_err(|e| {
            failed(
                MaterializeFailureKind::Pull,
                format!("resolve docker manifest for `{image_uri}`: {e}"),
            )
        })?;
    let compressed_bytes: u64 = manifest.layers.iter().map(|l| l.size).sum();
    let cap = max_image_bytes();
    if compressed_bytes > cap {
        return Err(failed(
            MaterializeFailureKind::TooLarge,
            format!(
                "`{image_uri}` sums {compressed_bytes} compressed layer bytes, over the \
                 enable-time cap of {cap} ({MAX_IMAGE_BYTES_ENV})"
            ),
        ));
    }

    // ---- scratch headroom (statvfs on the scratch volume) ----
    tokio::fs::create_dir_all(scratch).await.map_err(|e| {
        failed(
            MaterializeFailureKind::Internal,
            format!("create materialize scratch {}: {e}", scratch.display()),
        )
    })?;
    let needed = estimated_peak_scratch_bytes(compressed_bytes);
    if let Some(free) = crate::idle_evictor::free_disk_bytes(scratch) {
        if free < needed {
            return Err(failed(
                MaterializeFailureKind::DiskFull,
                format!(
                    "insufficient scratch headroom on {}: {free} bytes free < {needed} \
                     estimated peak (~2.5× the {compressed_bytes} compressed image bytes)",
                    scratch.display()
                ),
            ));
        }
    }
    // A failed statvfs falls through (fail open, same posture as the
    // idle evictor) — the materializer itself still fails loud on ENOSPC.

    // ---- keepalive: forward stage frames + re-send the last one ----
    // The materializer signals honest stage TRANSITIONS only; a long
    // silent leg (a multi-GiB layer pull, a slow mke2fs) would starve
    // the coordinator's lease renewal, so re-send the latest frame
    // every 20 s (comfortably under the ≤30 s contract) — the exact
    // shape of the capture path's `spawn_leg_keepalive`.
    let (stage_tx, mut stage_rx) = tokio::sync::mpsc::channel::<MaterializeProgress>(32);
    let keepalive = {
        let outer = progress.clone();
        tokio::spawn(async move {
            let mut last: Option<MaterializeProgress> = None;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(20));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // consume the immediate first tick
            loop {
                tokio::select! {
                    frame = stage_rx.recv() => match frame {
                        Some(f) => {
                            let _ = outer.try_send(f.clone());
                            last = Some(f);
                        }
                        // All senders dropped — the materialize
                        // returned; stop (and drop our progress clone
                        // so the RPC layer sees the channel close).
                        None => break,
                    },
                    _ = tick.tick() => {
                        if let Some(f) = &last {
                            let _ = outer.try_send(f.clone());
                        }
                    }
                }
            }
        })
    };

    // ---- run the pipeline ----
    let materializer = Materializer::new(
        oci,
        InitInjection {
            // Reserved in-guest agentd port; hard-coded here and in
            // engram-sandbox-firecracker::ENGRAM_AGENTD_PORT (same
            // convention as the retiring bake CLI).
            vsock_port: engram_sandbox_firecracker::ENGRAM_AGENTD_PORT,
            transport: Transport::Vsock,
            init_script: None,
        },
    );
    let result = materializer
        .materialize(image_uri, platform, scratch, chunk_store, Some(stage_tx))
        .await;
    // Await the keepalive so our clone of `progress` is dropped before
    // we return — the RPC handler relies on the progress channel
    // closing strictly before the terminal frame is computed.
    let _ = keepalive.await;

    let out = result.map_err(|e| map_materialize_error(image_uri, e))?;
    Ok(MaterializedImage {
        disk_manifest: out.disk_manifest,
        oci_defaults: out.oci_defaults,
        manifest_digest: out.manifest_digest,
        ext4_size_bytes: out.ext4_size_bytes,
    })
}

/// Classify a pipeline error into the wire's failure kinds — the
/// coordinator's retry policy hinges on this mapping (see
/// `MaterializeFailureKind::is_retryable`).
fn map_materialize_error(image_uri: &str, e: MaterializeError) -> SandboxError {
    let kind = match &e {
        // Deterministic image-content problems: retrying re-downloads
        // the same bytes for nothing.
        MaterializeError::Pull(PullError::UnsupportedLayerMediaType(_))
        | MaterializeError::Pull(PullError::Config(_))
        | MaterializeError::Flatten(_)
        | MaterializeError::Ext4(_) => MaterializeFailureKind::Image,
        // Registry transport: transient blips dominate (the enqueue
        // probe already rejected bad URIs/auth).
        MaterializeError::Pull(PullError::Oci(_)) | MaterializeError::Pull(PullError::Io(_)) => {
            MaterializeFailureKind::Pull
        }
        // BlobStorage/chunk-store writes: transient (GCS).
        MaterializeError::ChunkStore(_) => MaterializeFailureKind::Store,
        MaterializeError::Io(_) => MaterializeFailureKind::Internal,
    };
    failed(kind, format!("materialize `{image_uri}`: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_mapping_matches_the_retry_contract() {
        // Deterministic content problem → Image (bail fast).
        let e = map_materialize_error(
            "r/img:t",
            MaterializeError::Pull(PullError::UnsupportedLayerMediaType(
                "application/x-nope".into(),
            )),
        );
        match e {
            SandboxError::MaterializeFailed(f) => {
                assert_eq!(f.kind, MaterializeFailureKind::Image);
                assert!(!f.kind.is_retryable());
            }
            other => panic!("wrong error shape: {other}"),
        }
        // Registry transport → Pull (retryable).
        let e = map_materialize_error(
            "r/img:t",
            MaterializeError::Pull(PullError::Oci(engram_oci::OciError::Distribution(
                "connection reset".into(),
            ))),
        );
        match e {
            SandboxError::MaterializeFailed(f) => {
                assert_eq!(f.kind, MaterializeFailureKind::Pull);
                assert!(f.kind.is_retryable());
            }
            other => panic!("wrong error shape: {other}"),
        }
    }

    #[test]
    fn reconcile_scratch_sweeps_orphans_and_tolerates_absence() {
        let tmp = tempfile::tempdir().unwrap();
        let scratch = tmp.path().join("materialize-scratch");
        // Absent dir: no-op, no panic.
        reconcile_scratch(&scratch);
        // Orphaned per-run subdir with content: removed.
        let orphan = scratch.join("materialize-deadbeef");
        std::fs::create_dir_all(orphan.join("layers")).unwrap();
        std::fs::write(orphan.join("layers/0000.layer"), b"x").unwrap();
        reconcile_scratch(&scratch);
        assert!(!orphan.exists(), "orphaned scratch must be swept");
        assert!(scratch.exists(), "the scratch ROOT survives the sweep");
    }
}
