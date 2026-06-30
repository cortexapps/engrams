//! ADR 0062 §5: the harness catalog registration orchestration.
//!
//! Registering a harness (built-in or custom — both are OCI artifacts) is
//! synchronous and idempotent by content:
//!
//! 1. Pull the OCI artifact, extract its tree, validate its `harness.toml`
//!    (ADR 0063) and that the launch entry exists.
//! 2. Content-address the extracted tree and publish it as a tarball blob
//!    (`harness-trees/sha256/<tree_sha>`), so the catalog can be re-packed
//!    without re-pulling from the registry.
//! 3. Upsert the `harness_catalog` row.
//! 4. Re-pack the **whole** catalog squashfs (every live harness under
//!    `<name>/`) via [`crate::harness_pack`], publish it
//!    (`bundles/sha256/<gen_sha>`), and record it as the current generation —
//!    the single drive that mounts on `dyn_0`, identical across all sessions.
//!
//! Delete soft-deletes the row and re-packs from the remaining live harnesses;
//! the orphaned generation is reclaimed by the existing bundle GC once unpinned.

use std::path::Path;

use engram_core::types::harness::HarnessDescriptor;
use engram_core::types::sandbox::AuxRoDrive;
use engram_core::types::CatalogHarness;
use sha2::{Digest, Sha256};

use crate::error::ApiError;
use crate::harness_pack::{self, HarnessTree};
use crate::skill_pack::validate_skill_name;
use crate::state::SharedState;

/// BlobStorage key namespace for a harness's extracted-tree tarball.
fn tree_blob_key(tree_sha256: &str) -> String {
    format!("harness-trees/sha256/{tree_sha256}")
}

/// Register (or re-register) a harness from an OCI artifact, then re-pack +
/// publish the catalog generation.
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

    // Content-address the extracted tree + publish it as a tarball blob.
    let (tree_sha256, tree_tar) = hash_and_tar_dir(&dest)
        .map_err(|e| ApiError::Internal(format!("tar harness tree: {e}")))?;
    let tree_size_bytes = tree_tar.len() as i64;
    state
        .services
        .blob
        .put(&tree_blob_key(&tree_sha256), tree_tar.into())
        .await
        .map_err(|e| ApiError::Internal(format!("publish harness tree blob: {e}")))?;

    // Upsert the catalog row (now folds into bundle_pin_set via the generation).
    let row = state
        .services
        .meta
        .register_harness(engram_core::types::HarnessRegistration {
            owner: owner.trim(),
            name,
            oci_ref,
            manifest_digest: pulled.manifest_digest.as_str(),
            descriptor_toml: &descriptor_toml,
            tree_sha256: &tree_sha256,
            tree_size_bytes,
        })
        .await
        .map_err(ApiError::from)?;

    repack_and_publish(state).await?;

    tracing::info!(
        harness = %row.name,
        oci_ref = %row.oci_ref,
        tree_sha256 = %row.tree_sha256,
        owner = %row.owner,
        "registered harness",
    );
    Ok(row)
}

/// Soft-delete a harness, then re-pack the catalog from the remaining live rows.
pub async fn delete_harness(state: &SharedState, name: &str) -> Result<bool, ApiError> {
    let deleted = state
        .services
        .meta
        .soft_delete_harness(name)
        .await
        .map_err(ApiError::from)?;
    if deleted {
        repack_and_publish(state).await?;
    }
    Ok(deleted)
}

/// Materialize every live harness's tree from its blob, pack the catalog
/// squashfs, publish it, and record it as the current generation. A no-op when
/// no harnesses are live (the old generation stays until its session pins drain
/// and the bundle GC reclaims it).
pub async fn repack_and_publish(state: &SharedState) -> Result<(), ApiError> {
    let rows = state
        .services
        .meta
        .list_harnesses()
        .await
        .map_err(ApiError::from)?;
    if rows.is_empty() {
        return Ok(());
    }

    let staging = tempfile::tempdir().map_err(|e| ApiError::Internal(format!("tempdir: {e}")))?;
    let mut trees = Vec::with_capacity(rows.len());
    for row in &rows {
        let tar = state
            .services
            .blob
            .get(&tree_blob_key(&row.tree_sha256))
            .await
            .map_err(|e| {
                ApiError::Internal(format!(
                    "fetch harness tree {} for {}: {e}",
                    row.tree_sha256, row.name
                ))
            })?;
        let dir = staging.path().join(&row.name);
        untar_into(&tar, &dir)
            .map_err(|e| ApiError::Internal(format!("unpack harness tree {}: {e}", row.name)))?;
        trees.push(HarnessTree {
            name: row.name.clone(),
            dir,
        });
    }

    let packed = harness_pack::pack_harness_catalog(&trees)
        .map_err(|e| ApiError::Internal(format!("pack harness catalog: {e}")))?;
    state
        .services
        .blob
        .put(
            &AuxRoDrive::blob_key(&packed.sha256),
            packed.squashfs.into(),
        )
        .await
        .map_err(|e| ApiError::Internal(format!("publish harness catalog blob: {e}")))?;
    state
        .services
        .meta
        .set_harness_catalog_generation(&packed.sha256, packed.size_bytes)
        .await
        .map_err(ApiError::from)?;

    tracing::info!(
        generation = %packed.sha256,
        harnesses = rows.len(),
        size_bytes = packed.size_bytes,
        "re-packed harness catalog generation",
    );
    Ok(())
}

/// Deterministically hash + tar a directory tree (regular files only; parent
/// dirs are reconstructed at unpack). Sorted walk + zeroed mtime/uid/gid so
/// identical content yields an identical `tree_sha256` — re-registering the same
/// harness reuses the same blob (idempotent, no orphaned tree blobs).
fn hash_and_tar_dir(dir: &Path) -> Result<(String, Vec<u8>), String> {
    let mut files: Vec<(String, u32, Vec<u8>)> = Vec::new();
    collect_files(dir, dir, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    let mut builder = tar::Builder::new(Vec::new());
    for (rel, mode, content) in &files {
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        hasher.update(mode.to_le_bytes());
        hasher.update((content.len() as u64).to_le_bytes());
        hasher.update(content);

        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(*mode);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_cksum();
        builder
            .append_data(&mut header, rel, content.as_slice())
            .map_err(|e| format!("append {rel}: {e}"))?;
    }
    let tar = builder
        .into_inner()
        .map_err(|e| format!("finish tar: {e}"))?;
    Ok((format!("{:x}", hasher.finalize()), tar))
}

fn collect_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, u32, Vec<u8>)>,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let entries = std::fs::read_dir(dir).map_err(|e| format!("readdir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("dirent: {e}"))?;
        let path = entry.path();
        let ft = entry.file_type().map_err(|e| format!("filetype: {e}"))?;
        if ft.is_dir() {
            collect_files(root, &path, out)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|_| "strip_prefix".to_string())?
                .to_str()
                .ok_or_else(|| format!("non-utf8 path {}", path.display()))?
                .to_string();
            let mode = entry
                .metadata()
                .map_err(|e| format!("metadata: {e}"))?
                .permissions()
                .mode();
            let content =
                std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
            out.push((rel, mode, content));
        } else {
            return Err(format!(
                "unsupported file type at {} (only regular files + dirs allowed)",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Unpack a tarball into `dest` (creating it), preserving permissions.
fn untar_into(tar: &[u8], dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
    let mut ar = tar::Archive::new(tar);
    ar.set_preserve_permissions(true);
    ar.set_unpack_xattrs(false);
    ar.unpack(dest).map_err(|e| format!("unpack: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_tar_round_trips_and_is_deterministic() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("bin")).unwrap();
        std::fs::write(src.path().join("harness"), b"#!/bin/sh\nexec claude\n").unwrap();
        std::fs::write(src.path().join("harness.toml"), b"name = \"claude\"\n").unwrap();
        std::fs::write(src.path().join("bin/claude"), b"binary").unwrap();

        let (sha_a, tar_a) = hash_and_tar_dir(src.path()).unwrap();
        let (sha_b, tar_b) = hash_and_tar_dir(src.path()).unwrap();
        // Deterministic: identical tree → identical sha + bytes.
        assert_eq!(sha_a, sha_b);
        assert_eq!(tar_a, tar_b);
        assert_eq!(sha_a.len(), 64);

        // Round-trips the tree (paths + content) through unpack.
        let dst = tempfile::tempdir().unwrap();
        untar_into(&tar_a, dst.path()).unwrap();
        assert_eq!(
            std::fs::read(dst.path().join("harness.toml")).unwrap(),
            b"name = \"claude\"\n"
        );
        assert_eq!(
            std::fs::read(dst.path().join("bin/claude")).unwrap(),
            b"binary"
        );
    }

    #[test]
    fn tree_sha_changes_with_content() {
        let a = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("harness"), b"one").unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(b.path().join("harness"), b"two").unwrap();
        assert_ne!(
            hash_and_tar_dir(a.path()).unwrap().0,
            hash_and_tar_dir(b.path()).unwrap().0
        );
    }
}
