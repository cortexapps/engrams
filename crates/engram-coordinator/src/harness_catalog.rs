//! ADR 0062 §5: registering a **custom** harness from an OCI artifact.
//!
//! (Built-in harnesses — `claude` — do not come through here: they ride the
//! host-image `current_bundles` stamp + the coordinator's embedded descriptor,
//! see [`crate::builtin_harness`]. This module is the uploaded-skill analogue for
//! harnesses.) Registration is synchronous and idempotent by content:
//!
//! 1. Pull the OCI artifact, extract its tree, validate its `harness.toml`
//!    (ADR 0063) and that the launch entry exists.
//! 2. Pack that **one** harness tree into its own reproducible RO squashfs and
//!    publish it to `bundles/sha256/<sha>` (content-addressed by the squashfs
//!    bytes, `SOURCE_DATE_EPOCH=0`).
//! 3. Upsert the `harness_catalog` row carrying that `squashfs_sha256`.
//!
//! A session that selects the harness mounts that squashfs on `dyn_0`; the host
//! materializes it on demand exactly like an uploaded skill. Delete soft-deletes
//! the row; the squashfs leaves the pin set and the bundle GC reclaims it.

use engram_core::types::harness::HarnessDescriptor;
use engram_core::types::sandbox::AuxRoDrive;
use engram_core::types::CatalogHarness;
use sha2::{Digest, Sha256};

use crate::error::ApiError;
use crate::skill_pack::validate_skill_name;
use crate::state::SharedState;

/// Register (or re-register) a **custom** harness from an OCI artifact: pull it,
/// validate its `harness.toml`, pack its tree into its own RO squashfs, publish
/// that to `bundles/sha256/<sha>`, and upsert the catalog row. (Built-in harnesses
/// never come through here — they ride the host-image stamp + the embedded
/// descriptor; see [`crate::builtin_harness`].)
pub async fn register_harness(
    state: &SharedState,
    name: &str,
    oci_ref: &str,
    owner: &str,
) -> Result<CatalogHarness, ApiError> {
    let name = name.trim();
    validate_skill_name(name).map_err(|e| ApiError::BadRequest(format!("harness name: {e}")))?;
    let oci_ref = oci_ref.trim();
    if oci_ref.is_empty() {
        return Err(ApiError::BadRequest(
            "harness oci_ref must not be empty".into(),
        ));
    }

    // Pull the OCI artifact into a temp tree.
    let tmp = tempfile::tempdir().map_err(|e| ApiError::Internal(format!("tempdir: {e}")))?;
    let dest = tmp.path().join("tree");
    let pulled = state
        .services
        .oci
        .pull_harness(oci_ref, &dest)
        .await
        .map_err(|e| ApiError::BadRequest(format!("pull harness OCI {oci_ref}: {e}")))?;

    // Validate the harness.toml the artifact ships at its root (ADR 0063).
    let descriptor_toml = tokio::fs::read_to_string(dest.join("harness.toml"))
        .await
        .map_err(|_| {
            ApiError::BadRequest("harness OCI artifact is missing harness.toml at its root".into())
        })?;
    let descriptor = HarnessDescriptor::parse(&descriptor_toml)
        .map_err(|e| ApiError::BadRequest(format!("harness.toml: {e}")))?;
    if descriptor.name != name {
        return Err(ApiError::BadRequest(format!(
            "harness.toml name {:?} does not match the requested name {name:?}",
            descriptor.name
        )));
    }
    // Defence-in-depth: the launch entry must exist in the pulled tree, so a
    // session that selects this harness can actually exec it from the catalog.
    if !dest.join(descriptor.exec_path()).is_file() {
        return Err(ApiError::BadRequest(format!(
            "harness entry {:?} not found in the OCI artifact tree",
            descriptor.exec_path()
        )));
    }

    // A custom harness must not shadow a built-in (the resolver checks built-ins
    // first, so a shadowing row would be unreachable + confusing).
    if crate::builtin_harness::builtin(name).is_some() {
        return Err(ApiError::BadRequest(format!(
            "{name:?} is a built-in harness and cannot be registered as a custom one"
        )));
    }

    // Pack this one harness tree into its own reproducible RO squashfs and publish
    // it under `bundles/sha256/<sha>` (content-addressed by the squashfs bytes,
    // SOURCE_DATE_EPOCH=0). Hosts materialize it on demand exactly like an
    // uploaded skill; no "catalog generation" to re-pack.
    let squashfs = crate::squashfs::pack_dir(&dest)
        .map_err(|e| ApiError::Internal(format!("pack harness squashfs: {e}")))?;
    let squashfs_size_bytes = squashfs.len() as i64;
    let squashfs_sha256 = hex::encode(Sha256::digest(&squashfs));
    state
        .services
        .blob
        .put(&AuxRoDrive::blob_key(&squashfs_sha256), squashfs.into())
        .await
        .map_err(|e| ApiError::Internal(format!("publish harness squashfs blob: {e}")))?;

    let row = state
        .services
        .meta
        .register_harness(engram_core::types::HarnessRegistration {
            owner: owner.trim(),
            name,
            oci_ref,
            manifest_digest: pulled.manifest_digest.as_str(),
            descriptor_toml: &descriptor_toml,
            squashfs_sha256: &squashfs_sha256,
            squashfs_size_bytes,
        })
        .await
        .map_err(ApiError::from)?;

    tracing::info!(
        harness = %row.name,
        oci_ref = %row.oci_ref,
        squashfs_sha256 = %row.squashfs_sha256,
        owner = %row.owner,
        "registered custom harness",
    );
    Ok(row)
}

/// Soft-delete a custom harness; its squashfs leaves the pin set and the bundle
/// GC reclaims it once unpinned.
pub async fn delete_harness(state: &SharedState, name: &str) -> Result<bool, ApiError> {
    state
        .services
        .meta
        .soft_delete_harness(name)
        .await
        .map_err(ApiError::from)
}
