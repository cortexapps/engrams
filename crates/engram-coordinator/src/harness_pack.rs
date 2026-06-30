//! ADR 0062 §5: pack the registered harness catalog into one deterministic,
//! content-addressed read-only squashfs.
//!
//! Every registered harness is laid out under `<name>/` (its extracted OCI tree:
//! the entry binary, any bundled runtime/CLI sidecars, and its `harness.toml`),
//! and a `mount.json` (`{"kind":"harness"}`) is written at the squashfs root. The
//! guest mounts this single squashfs on `dyn_0` and `exec`s the selected harness
//! at `/opt/engram/dyn/0/<name>/<exec>`. Because the drive content is identical
//! across all sessions at a given catalog version, it is staged once per host and
//! shared (dedup across sessions); selection lives entirely in `argv`.
//!
//! Packed via the shared [`crate::squashfs`] helper, so the same set of trees →
//! the same sha256 — the catalog *generation* id (blob key `bundles/sha256/<sha>`).
//! This module only packs; validating each harness's `harness.toml` is the
//! registration step's job (ADR 0062 A3), not the packer's.

use std::path::{Path, PathBuf};

use engram_mount_manifest::MountManifest;
use sha2::{Digest, Sha256};

/// One harness to fold into the catalog: its catalog `name` and the local
/// directory holding its extracted tree.
pub struct HarnessTree {
    /// Catalog name == argv subtree == wire `CreateSessionRequest.harness`.
    pub name: String,
    /// Local directory whose contents become `<name>/` in the catalog squashfs.
    pub dir: PathBuf,
}

/// The packed harness catalog.
#[derive(Debug)]
pub struct PackedCatalog {
    /// The packed squashfs bytes (publish to `bundles/sha256/<sha256>`).
    pub squashfs: Vec<u8>,
    /// Content address of `squashfs` (lowercase hex sha256) — the catalog
    /// generation id.
    pub sha256: String,
    /// `squashfs.len()`.
    pub size_bytes: i64,
}

/// Why packing failed. `Invalid` is a caller error (bad/duplicate name, missing
/// tree); `Internal` is a tooling/IO failure.
#[derive(Debug)]
pub enum CatalogPackError {
    Invalid(String),
    Internal(String),
}

impl std::fmt::Display for CatalogPackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogPackError::Invalid(m) => write!(f, "invalid harness catalog: {m}"),
            CatalogPackError::Internal(m) => write!(f, "harness catalog pack failed: {m}"),
        }
    }
}

impl std::error::Error for CatalogPackError {}

/// Pack `harnesses` into a single content-addressed catalog squashfs. Each
/// harness's `dir` tree is copied under `<name>/`; a root `mount.json`
/// (`{"kind":"harness"}`) is written so the guest skips the slot during
/// `activate()`. Names are validated (a single safe path segment, reusing the
/// skill-name rule) and must be unique. Determinism comes from the shared packer
/// pinning ownership + timestamps, so an unchanged catalog re-packs to the same
/// sha (idempotent staging).
pub fn pack_harness_catalog(harnesses: &[HarnessTree]) -> Result<PackedCatalog, CatalogPackError> {
    if harnesses.is_empty() {
        return Err(CatalogPackError::Invalid("empty harness catalog".into()));
    }

    let staging =
        tempfile::tempdir().map_err(|e| CatalogPackError::Internal(format!("tempdir: {e}")))?;
    let root = staging.path();

    let mut seen = std::collections::HashSet::new();
    for h in harnesses {
        // A harness name becomes a subdir, an argv path component, and the wire
        // value, so it must be a single safe segment — the same rule skills use.
        crate::skill_pack::validate_skill_name(&h.name)
            .map_err(|e| CatalogPackError::Invalid(format!("harness name {:?}: {e}", h.name)))?;
        if !seen.insert(h.name.as_str()) {
            return Err(CatalogPackError::Invalid(format!(
                "duplicate harness name {:?}",
                h.name
            )));
        }
        if !h.dir.is_dir() {
            return Err(CatalogPackError::Invalid(format!(
                "harness {:?} tree {} is not a directory",
                h.name,
                h.dir.display()
            )));
        }
        copy_tree(&h.dir, &root.join(&h.name))?;
    }

    let mount_json = serde_json::to_string(&MountManifest::harness_catalog())
        .map_err(|e| CatalogPackError::Internal(format!("serialize mount.json: {e}")))?;
    std::fs::write(root.join("mount.json"), mount_json)
        .map_err(|e| CatalogPackError::Internal(format!("write mount.json: {e}")))?;

    let squashfs = crate::squashfs::pack_dir(root).map_err(CatalogPackError::Internal)?;
    let sha256 = format!("{:x}", Sha256::digest(&squashfs));
    let size_bytes = squashfs.len() as i64;
    Ok(PackedCatalog {
        squashfs,
        sha256,
        size_bytes,
    })
}

/// Recursively copy a directory tree, preserving structure and (Unix) mode bits
/// so the harness entry stays executable. Only regular files + dirs are copied;
/// anything else (symlink, device) is rejected as a safety guard — the OCI
/// extraction upstream should already have produced a clean tree.
fn copy_tree(src: &Path, dst: &Path) -> Result<(), CatalogPackError> {
    std::fs::create_dir_all(dst)
        .map_err(|e| CatalogPackError::Internal(format!("mkdir {}: {e}", dst.display())))?;
    let entries = std::fs::read_dir(src)
        .map_err(|e| CatalogPackError::Internal(format!("readdir {}: {e}", src.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| CatalogPackError::Internal(format!("dirent: {e}")))?;
        let ft = entry
            .file_type()
            .map_err(|e| CatalogPackError::Internal(format!("filetype: {e}")))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else if ft.is_file() {
            // std::fs::copy preserves the Unix permission bits, so an executable
            // entry stays executable in the staged tree (mksquashfs preserves the
            // tree mode; `-all-root` only rewrites ownership).
            std::fs::copy(&from, &to)
                .map_err(|e| CatalogPackError::Internal(format!("copy {}: {e}", from.display())))?;
        } else {
            return Err(CatalogPackError::Invalid(format!(
                "unsupported file type at {} (only regular files + dirs allowed)",
                from.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn mksquashfs_available() -> bool {
        Command::new("mksquashfs").arg("-version").output().is_ok()
    }

    /// Build a fake harness tree under `parent/<name>` with a `harness` entry +
    /// a `harness.toml`, returning the [`HarnessTree`].
    fn fake_harness(parent: &Path, name: &str) -> HarnessTree {
        let dir = parent.join(format!("src-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("harness"), format!("#!/bin/sh\necho {name}\n")).unwrap();
        std::fs::write(dir.join("harness.toml"), format!("name = \"{name}\"\n")).unwrap();
        HarnessTree {
            name: name.to_string(),
            dir,
        }
    }

    #[test]
    fn rejects_empty_and_duplicate_and_bad_names() {
        assert!(matches!(
            pack_harness_catalog(&[]),
            Err(CatalogPackError::Invalid(_))
        ));
        let tmp = tempfile::tempdir().unwrap();
        let a = fake_harness(tmp.path(), "claude");
        let b = HarnessTree {
            name: "claude".to_string(),
            dir: a.dir.clone(),
        };
        match pack_harness_catalog(&[a, b]) {
            Err(CatalogPackError::Invalid(m)) => assert!(m.contains("duplicate"), "{m}"),
            other => panic!("expected duplicate rejection, got {other:?}"),
        }
        let bad = fake_harness(tmp.path(), "claude");
        let bad = HarnessTree {
            name: "Has-Caps".to_string(),
            dir: bad.dir,
        };
        assert!(matches!(
            pack_harness_catalog(&[bad]),
            Err(CatalogPackError::Invalid(_))
        ));
    }

    #[test]
    fn packs_deterministically_and_content_addresses_the_set() {
        if !mksquashfs_available() {
            eprintln!("skipping packs_deterministically: mksquashfs not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        // Same set, packed twice → identical sha (idempotent staging).
        let a1 = pack_harness_catalog(&[
            fake_harness(tmp.path(), "claude"),
            fake_harness(tmp.path(), "opencode"),
        ])
        .expect("pack");
        let a2 = pack_harness_catalog(&[
            fake_harness(tmp.path(), "claude"),
            fake_harness(tmp.path(), "opencode"),
        ])
        .expect("pack");
        assert_eq!(a1.sha256, a2.sha256);
        assert_eq!(a1.sha256.len(), 64);
        assert!(a1.sha256.bytes().all(|c| c.is_ascii_hexdigit()));
        assert!(a1.size_bytes > 0);
        // squashfs superblock magic ("hsqs").
        assert_eq!(&a1.squashfs[0..4], b"hsqs");

        // A different set (one more harness) → a different generation sha.
        let b = pack_harness_catalog(&[
            fake_harness(tmp.path(), "claude"),
            fake_harness(tmp.path(), "opencode"),
            fake_harness(tmp.path(), "aider"),
        ])
        .expect("pack");
        assert_ne!(a1.sha256, b.sha256);
    }
}
