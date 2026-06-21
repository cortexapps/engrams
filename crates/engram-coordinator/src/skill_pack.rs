//! ADR 0055 P2: pack an uploaded skill directory into a deterministic,
//! content-addressed read-only squashfs bundle.
//!
//! This is the producer side of the `mount.json` contract
//! (`engram-mount-manifest`) that the in-guest `engram-session-bundles::activate()`
//! consumes. The uploaded tar is laid out under `skills/<name>/` and a generated
//! `mount.json` is written at the squashfs root — exactly the layout `activate()`
//! expects (it symlinks `skills/<name>` onto the harness's discovery path). The
//! tree is packed with `mksquashfs` under reproducible flags (the same tool the
//! P1 `deploy/bundles/*` recipes use); the output's sha256 is the content
//! address (blob key `bundles/sha256/<sha>`), so re-registering identical bytes
//! is idempotent.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use engram_mount_manifest::MountManifest;
use sha2::{Digest, Sha256};

/// Cap on the uploaded tar. Markdown skills are KiB; this is generous headroom
/// and stays under the 4 MiB gRPC default decode cap.
pub const MAX_SKILL_TAR_BYTES: usize = 2 * 1024 * 1024;

/// Largest number of files one skill may carry (a sanity bound, not a quota).
const MAX_SKILL_FILES: usize = 4096;

/// The result of packing an uploaded skill.
#[derive(Debug)]
pub struct PackedSkill {
    /// The packed squashfs bytes (publish to `bundles/sha256/<sha256>`).
    pub squashfs: Vec<u8>,
    /// Content address of `squashfs` (lowercase hex sha256).
    pub sha256: String,
    /// The `mount.json` written at the squashfs root (stored on the catalog row
    /// for the record).
    pub mount_json: String,
    /// `squashfs.len()`.
    pub size_bytes: i64,
}

/// Why a pack failed. `Invalid` is a client error (→ 400); `Internal` is a
/// server/tooling failure (→ 500).
#[derive(Debug)]
pub enum PackError {
    Invalid(String),
    Internal(String),
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackError::Invalid(m) => write!(f, "invalid skill upload: {m}"),
            PackError::Internal(m) => write!(f, "skill pack failed: {m}"),
        }
    }
}

impl std::error::Error for PackError {}

/// Validate a catalog/bundle name: a single safe path segment (it becomes the
/// `skills/<name>/` dir + the wire name). Lowercase alphanumerics, dash,
/// underscore; 1..=64 chars. Collision with a *fleet* bundle name is checked by
/// the caller against the live fleet stamp.
pub fn validate_skill_name(name: &str) -> Result<(), PackError> {
    if name.is_empty() || name.len() > 64 {
        return Err(PackError::Invalid("name must be 1..=64 chars".into()));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(PackError::Invalid(
            "name must be lowercase alphanumerics, dash, or underscore".into(),
        ));
    }
    Ok(())
}

/// Pack `payload_tar` (a plain or gzipped POSIX tar of the skill dir) into a
/// content-addressed squashfs for skill `name`.
pub fn pack_skill(name: &str, payload_tar: &[u8]) -> Result<PackedSkill, PackError> {
    validate_skill_name(name)?;
    if payload_tar.len() > MAX_SKILL_TAR_BYTES {
        return Err(PackError::Invalid(format!(
            "payload {} bytes exceeds the {MAX_SKILL_TAR_BYTES} byte cap",
            payload_tar.len()
        )));
    }

    let staging = tempfile::tempdir().map_err(|e| PackError::Internal(format!("tempdir: {e}")))?;
    let root = staging.path();
    let skill_dir = root.join("skills").join(name);
    std::fs::create_dir_all(&skill_dir)
        .map_err(|e| PackError::Internal(format!("mkdir skills/{name}: {e}")))?;

    extract_tar_into(payload_tar, &skill_dir)?;

    // Every skill must carry a top-level SKILL.md (the harness discovery doc).
    if !skill_dir.join("SKILL.md").is_file() {
        return Err(PackError::Invalid(
            "payload must contain a top-level SKILL.md".into(),
        ));
    }

    // Generate the mount.json the guest's activate() reads.
    let manifest = MountManifest::single_skill(name);
    let mount_json = serde_json::to_string(&manifest)
        .map_err(|e| PackError::Internal(format!("serialize mount.json: {e}")))?;
    std::fs::write(root.join("mount.json"), &mount_json)
        .map_err(|e| PackError::Internal(format!("write mount.json: {e}")))?;

    let squashfs = mksquashfs(root)?;
    let sha256 = format!("{:x}", Sha256::digest(&squashfs));
    let size_bytes = squashfs.len() as i64;
    Ok(PackedSkill {
        squashfs,
        sha256,
        mount_json,
        size_bytes,
    })
}

/// Extract a (possibly gzipped) tar into `dest`, rejecting unsafe entries.
fn extract_tar_into(payload: &[u8], dest: &Path) -> Result<(), PackError> {
    // Sniff the gzip magic (1f 8b) — accept either a plain or gzipped tar so the
    // orchestrator can forward a `.tar.gz` upload verbatim.
    let reader: Box<dyn Read> = if payload.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::GzDecoder::new(payload))
    } else {
        Box::new(payload)
    };
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(false);
    ar.set_unpack_xattrs(false);
    let entries = ar
        .entries()
        .map_err(|e| PackError::Invalid(format!("not a valid tar: {e}")))?;

    let mut count = 0usize;
    for entry in entries {
        let mut entry = entry.map_err(|e| PackError::Invalid(format!("tar entry: {e}")))?;
        let etype = entry.header().entry_type();
        // RO markdown/file skills need only regular files + dirs. Reject
        // symlinks/hardlinks/devices — they're a traversal/escape risk and have
        // no place in a skill bundle.
        if !(etype.is_file() || etype.is_dir()) {
            return Err(PackError::Invalid(format!(
                "tar entry has unsupported type {etype:?} (only files + dirs allowed)"
            )));
        }
        let path = entry
            .path()
            .map_err(|e| PackError::Invalid(format!("tar path: {e}")))?
            .into_owned();
        let safe = sanitize_rel(&path)?;
        let out = dest.join(&safe);
        if etype.is_dir() {
            std::fs::create_dir_all(&out)
                .map_err(|e| PackError::Internal(format!("mkdir {}: {e}", out.display())))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| PackError::Internal(format!("mkdir {}: {e}", parent.display())))?;
        }
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .map_err(|e| PackError::Invalid(format!("read tar entry {}: {e}", safe.display())))?;
        std::fs::write(&out, &buf)
            .map_err(|e| PackError::Internal(format!("write {}: {e}", out.display())))?;
        count += 1;
        if count > MAX_SKILL_FILES {
            return Err(PackError::Invalid(format!(
                "skill has more than {MAX_SKILL_FILES} files"
            )));
        }
    }
    if count == 0 {
        return Err(PackError::Invalid("empty skill payload".into()));
    }
    Ok(())
}

/// Reduce a tar entry path to a safe relative path, rejecting `..`, absolute,
/// and prefix components (no escape out of the skill dir).
fn sanitize_rel(path: &Path) -> Result<PathBuf, PackError> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(PackError::Invalid(format!(
                    "unsafe tar path component in {}",
                    path.display()
                )));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(PackError::Invalid("empty tar path".into()));
    }
    Ok(out)
}

/// Pack `tree` into a squashfs (reproducible flags) and return its bytes.
fn mksquashfs(tree: &Path) -> Result<Vec<u8>, PackError> {
    let out_dir = tempfile::tempdir().map_err(|e| PackError::Internal(format!("tempdir: {e}")))?;
    let out_path = out_dir.path().join("skill.squashfs");
    // Determinism (so identical content → identical sha, making re-registration
    // idempotent): `-all-root` (root-owned, as the guest mounts RO), `-no-xattrs`,
    // `-comp zstd` (matching the P1 `deploy/bundles/*/build.sh` recipes), and a
    // pinned `SOURCE_DATE_EPOCH` so mksquashfs clamps every timestamp to a fixed
    // value (the prod coordinator container has no ambient epoch; the nix dev
    // shell sets its own, so we override to a constant either way). We must NOT
    // also pass `-mkfs-time`/`-all-time` — mksquashfs refuses both at once.
    let output = Command::new("mksquashfs")
        .arg(tree)
        .arg(&out_path)
        .args(["-comp", "zstd", "-all-root", "-noappend", "-no-xattrs"])
        .env("SOURCE_DATE_EPOCH", "0")
        .output()
        .map_err(|e| {
            PackError::Internal(format!(
                "spawn mksquashfs (is squashfs-tools installed?): {e}"
            ))
        })?;
    if !output.status.success() {
        return Err(PackError::Internal(format!(
            "mksquashfs exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    std::fs::read(&out_path).map_err(|e| PackError::Internal(format!("read squashfs: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_with(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, body) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, path, *body).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn validate_name_accepts_safe_and_rejects_unsafe() {
        assert!(validate_skill_name("my-linter").is_ok());
        assert!(validate_skill_name("skill_2").is_ok());
        assert!(validate_skill_name("").is_err());
        assert!(validate_skill_name("Has-Caps").is_err());
        assert!(validate_skill_name("dots.bad").is_err());
        assert!(validate_skill_name("slash/bad").is_err());
        assert!(validate_skill_name("..").is_err());
        assert!(validate_skill_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn sanitize_rel_blocks_traversal() {
        assert!(sanitize_rel(Path::new("../etc/passwd")).is_err());
        assert!(sanitize_rel(Path::new("/abs")).is_err());
        assert!(sanitize_rel(Path::new("a/../../b")).is_err());
        assert_eq!(
            sanitize_rel(Path::new("./sub/file.md")).unwrap(),
            PathBuf::from("sub/file.md")
        );
    }

    #[test]
    fn pack_requires_skill_md() {
        let tar = tar_with(&[("README.md", b"no skill doc here")]);
        let err = pack_skill("my-skill", &tar).unwrap_err();
        assert!(matches!(err, PackError::Invalid(_)), "got {err:?}");
    }

    #[test]
    fn pack_rejects_oversize() {
        let big = vec![0u8; MAX_SKILL_TAR_BYTES + 1];
        assert!(matches!(
            pack_skill("my-skill", &big),
            Err(PackError::Invalid(_))
        ));
    }

    // Exercises mksquashfs end to end. Runs wherever squashfs-tools is on PATH —
    // the nix dev shell (`just check`) and CI both provide it (flake.nix). Fails
    // loud rather than silently skipping if the tool is missing.
    #[test]
    fn pack_is_deterministic_and_well_formed() {
        let tar = tar_with(&[
            ("SKILL.md", b"# My Skill\nDo the thing.\n"),
            ("reference/notes.md", b"notes\n"),
        ]);
        let a = pack_skill("my-skill", &tar).expect("pack a");
        let b = pack_skill("my-skill", &tar).expect("pack b");
        // Idempotent by content: identical input → identical sha256.
        assert_eq!(a.sha256, b.sha256);
        assert_eq!(a.sha256.len(), 64);
        assert!(a.sha256.bytes().all(|c| c.is_ascii_hexdigit()));
        assert!(a.size_bytes > 0);
        // The generated manifest matches the activate() shape.
        assert_eq!(
            a.mount_json,
            r#"{"kind":"skill","skills":[{"name":"my-skill"}]}"#
        );
        // squashfs superblock magic ("hsqs", little-endian 0x73717368).
        assert_eq!(&a.squashfs[0..4], b"hsqs");
    }
}
