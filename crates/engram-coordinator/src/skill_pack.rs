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

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use engram_mount_manifest::MountManifest;
use sha2::{Digest, Sha256};

/// Cap on the *compressed* upload (tar / tar.gz / zip). Markdown skills are KiB;
/// this is generous headroom and stays under the 4 MiB gRPC default decode cap.
/// Checked before any decompression, so it bounds the work an attacker's bytes
/// can trigger up front.
pub const MAX_SKILL_UPLOAD_BYTES: usize = 2 * 1024 * 1024;

/// Cap on the *decompressed* total written across all entries — the
/// decompression-bomb (zip/gzip bomb) defense. Enforced by streaming each entry
/// through this budget in fixed chunks, so memory never exceeds it regardless of
/// an entry's *claimed* size (zip/tar headers can lie), and a 2 MiB upload that
/// would expand to gigabytes is aborted partway.
const MAX_SKILL_UNPACKED_BYTES: u64 = 16 * 1024 * 1024;

/// Largest number of files one skill may carry — a metadata/inode-bomb bound
/// (many tiny entries) on top of the byte budget.
const MAX_SKILL_FILES: usize = 4096;

/// Magic bytes for a zip local-file header (`PK\x03\x04`).
const ZIP_MAGIC: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
/// Magic bytes for a gzip stream (`1f 8b`).
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
/// The `ustar` magic in a POSIX/GNU tar header (offset 257).
const TAR_USTAR_OFFSET: usize = 257;
const TAR_USTAR_MAGIC: &[u8] = b"ustar";

/// `true` if `payload` looks like a (plain, uncompressed) tar — its header
/// carries the `ustar` magic at offset 257. A lone markdown doc won't.
fn looks_like_tar(payload: &[u8]) -> bool {
    payload.get(TAR_USTAR_OFFSET..TAR_USTAR_OFFSET + TAR_USTAR_MAGIC.len()) == Some(TAR_USTAR_MAGIC)
}

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

/// Pack `payload` — a tar, gzipped tar, or zip of the skill dir, or a lone
/// `SKILL.md` (all sniffed by magic bytes) — into a content-addressed squashfs
/// for skill `name`.
pub fn pack_skill(name: &str, payload: &[u8]) -> Result<PackedSkill, PackError> {
    validate_skill_name(name)?;
    if payload.len() > MAX_SKILL_UPLOAD_BYTES {
        return Err(PackError::Invalid(format!(
            "payload {} bytes exceeds the {MAX_SKILL_UPLOAD_BYTES} byte cap",
            payload.len()
        )));
    }

    let staging = tempfile::tempdir().map_err(|e| PackError::Internal(format!("tempdir: {e}")))?;
    let root = staging.path();
    let skill_dir = root.join("skills").join(name);
    std::fs::create_dir_all(&skill_dir)
        .map_err(|e| PackError::Internal(format!("mkdir skills/{name}: {e}")))?;

    extract_into(payload, &skill_dir)?;

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

/// Normalize an upload into the staging `dest`. Sniffed by **magic bytes**, never
/// the filename: zip (`PK\x03\x04`) → zip; gzip (`1f 8b`) or a `ustar` header →
/// (gzipped) tar; **otherwise the whole payload is a lone `SKILL.md`** — the
/// common case where a user uploads just their skill doc, so the coordinator is
/// the single normalization point and the orchestrator forwards raw bytes.
/// Archive entries stream through a shared decompressed-byte budget (the bomb
/// defense) and unsafe entries are rejected.
fn extract_into(payload: &[u8], dest: &Path) -> Result<(), PackError> {
    if payload.is_empty() {
        return Err(PackError::Invalid("empty skill payload".into()));
    }
    let mut budget = MAX_SKILL_UNPACKED_BYTES;
    let count = if payload.starts_with(&ZIP_MAGIC) {
        extract_zip_into(payload, dest, &mut budget)?
    } else if payload.starts_with(&GZIP_MAGIC) || looks_like_tar(payload) {
        extract_tar_into(payload, dest, &mut budget)?
    } else {
        // A lone file → it IS the SKILL.md (bounded by the same byte budget).
        write_bounded(&mut &payload[..], &dest.join("SKILL.md"), &mut budget)?;
        1
    };
    if count == 0 {
        return Err(PackError::Invalid("empty skill payload".into()));
    }
    Ok(())
}

/// Extract a plain or gzipped tar. Returns the file count.
fn extract_tar_into(payload: &[u8], dest: &Path, budget: &mut u64) -> Result<usize, PackError> {
    let reader: Box<dyn Read> = if payload.starts_with(&GZIP_MAGIC) {
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
        write_bounded(&mut entry, &out, budget)?;
        count += 1;
        if count > MAX_SKILL_FILES {
            return Err(PackError::Invalid(format!(
                "skill has more than {MAX_SKILL_FILES} files"
            )));
        }
    }
    Ok(count)
}

/// Extract a zip. Returns the file count. Rejects symlink entries and unsafe
/// paths; trusts neither the declared entry size nor the filename.
fn extract_zip_into(payload: &[u8], dest: &Path, budget: &mut u64) -> Result<usize, PackError> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(payload))
        .map_err(|e| PackError::Invalid(format!("not a valid zip: {e}")))?;

    let mut count = 0usize;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| PackError::Invalid(format!("zip entry {i}: {e}")))?;

        // Reject symlinks (unix mode S_IFLNK) — an escape risk, like tar.
        if let Some(mode) = entry.unix_mode() {
            if mode & 0o170000 == 0o120000 {
                return Err(PackError::Invalid(
                    "zip entry is a symlink (only files + dirs allowed)".into(),
                ));
            }
        }
        // `enclosed_name` returns None for absolute / `..`-escaping paths; we
        // re-sanitize on top as defense in depth.
        let Some(name) = entry.enclosed_name() else {
            return Err(PackError::Invalid(format!(
                "unsafe zip path: {}",
                entry.name()
            )));
        };
        let safe = sanitize_rel(&name)?;
        let out = dest.join(&safe);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)
                .map_err(|e| PackError::Internal(format!("mkdir {}: {e}", out.display())))?;
            continue;
        }
        write_bounded(&mut entry, &out, budget)?;
        count += 1;
        if count > MAX_SKILL_FILES {
            return Err(PackError::Invalid(format!(
                "skill has more than {MAX_SKILL_FILES} files"
            )));
        }
    }
    Ok(count)
}

/// Stream `reader` to `out_path` (creating parent dirs), debiting the shared
/// decompressed-byte `budget`. Fixed-chunk copy so memory stays bounded
/// regardless of a (possibly bomb) entry's size; aborts the instant the total
/// would exceed the budget — the decompressor is never driven past the cap.
fn write_bounded(
    reader: &mut impl Read,
    out_path: &Path,
    budget: &mut u64,
) -> Result<(), PackError> {
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| PackError::Internal(format!("mkdir {}: {e}", parent.display())))?;
    }
    let mut file = std::fs::File::create(out_path)
        .map_err(|e| PackError::Internal(format!("create {}: {e}", out_path.display())))?;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|e| PackError::Invalid(format!("read entry: {e}")))?;
        if n == 0 {
            break;
        }
        if (n as u64) > *budget {
            return Err(PackError::Invalid(format!(
                "skill exceeds the {MAX_SKILL_UNPACKED_BYTES} byte unpacked cap \
                 (decompression bomb?)"
            )));
        }
        *budget -= n as u64;
        file.write_all(&chunk[..n])
            .map_err(|e| PackError::Internal(format!("write {}: {e}", out_path.display())))?;
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
        let big = vec![0u8; MAX_SKILL_UPLOAD_BYTES + 1];
        assert!(matches!(
            pack_skill("my-skill", &big),
            Err(PackError::Invalid(_))
        ));
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    fn zip_with(files: &[(&str, &[u8])], method: zip::CompressionMethod) -> Vec<u8> {
        use std::io::Write as _;
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default().compression_method(method);
        for (name, body) in files {
            w.start_file(*name, opts).unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn rejects_a_gzip_tar_bomb() {
        // A tar whose one entry decompresses far past the unpacked cap, gzipped
        // small enough to clear the compressed-input cap — the classic bomb. The
        // budget aborts extraction before mksquashfs is ever reached (so this
        // runs without squashfs-tools).
        let huge = vec![0u8; (MAX_SKILL_UNPACKED_BYTES as usize) + 1024 * 1024];
        let bomb = gzip(&tar_with(&[("SKILL.md", &huge)]));
        assert!(
            bomb.len() < MAX_SKILL_UPLOAD_BYTES,
            "bomb clears the input cap"
        );
        match pack_skill("bomb", &bomb) {
            Err(PackError::Invalid(m)) => assert!(m.contains("unpacked cap"), "got {m}"),
            other => panic!("expected an unpacked-cap rejection, got {other:?}"),
        }
    }

    #[test]
    fn rejects_a_zip_bomb() {
        let huge = vec![0u8; (MAX_SKILL_UNPACKED_BYTES as usize) + 1024 * 1024];
        let bomb = zip_with(&[("SKILL.md", &huge)], zip::CompressionMethod::Deflated);
        assert!(
            bomb.len() < MAX_SKILL_UPLOAD_BYTES,
            "zip bomb clears the input cap"
        );
        match pack_skill("zbomb", &bomb) {
            Err(PackError::Invalid(m)) => assert!(m.contains("unpacked cap"), "got {m}"),
            other => panic!("expected an unpacked-cap rejection, got {other:?}"),
        }
    }

    #[test]
    fn pack_accepts_a_lone_skill_md() {
        if !mksquashfs_available() {
            eprintln!("skipping pack_accepts_a_lone_skill_md: mksquashfs not on PATH");
            return;
        }
        // A raw markdown payload (no archive magic) is the SKILL.md itself, and
        // packs identically to a one-entry tar carrying the same SKILL.md — so
        // the orchestrator can forward raw bytes and dedup still holds.
        let doc = b"# My Skill\nDo the thing.\n";
        let lone = pack_skill("x", doc).expect("lone");
        let tarred = pack_skill("x", &tar_with(&[("SKILL.md", doc)])).expect("tar");
        assert_eq!(lone.sha256, tarred.sha256);
        assert_eq!(&lone.squashfs[0..4], b"hsqs");
    }

    #[test]
    fn rejects_an_empty_payload() {
        assert!(matches!(pack_skill("x", b""), Err(PackError::Invalid(_))));
    }

    #[test]
    fn zip_and_tar_pack_to_the_same_content_address() {
        if !mksquashfs_available() {
            eprintln!(
                "skipping zip_and_tar_pack_to_the_same_content_address: mksquashfs not on PATH"
            );
            return;
        }
        // A .zip and a .tar carrying byte-identical files pack to the SAME
        // squashfs — the content address keys on the unpacked tree, not the
        // archive format (so a user can upload either and dedup still holds).
        let files: &[(&str, &[u8])] = &[
            ("SKILL.md", b"# Zip Skill\nhi\n"),
            ("reference/notes.md", b"notes\n"),
        ];
        let from_zip =
            pack_skill("z", &zip_with(files, zip::CompressionMethod::Stored)).expect("zip");
        let from_tar = pack_skill("z", &tar_with(files)).expect("tar");
        assert_eq!(from_zip.sha256, from_tar.sha256);
        assert_eq!(&from_zip.squashfs[0..4], b"hsqs");
    }

    fn mksquashfs_available() -> bool {
        Command::new("mksquashfs").arg("-version").output().is_ok()
    }

    // Exercises mksquashfs end to end. Runs wherever squashfs-tools is on PATH —
    // the nix dev shell (`just check`) + the `test-linux` CI lane (which apt-
    // installs it). Self-skips elsewhere (e.g. the macOS lane), mirroring the
    // ext4-determinism test's mke2fs gate, so it still runs in CI but doesn't
    // hard-fail a toolless environment.
    #[test]
    fn pack_is_deterministic_and_well_formed() {
        if !mksquashfs_available() {
            eprintln!("skipping pack_is_deterministic_and_well_formed: mksquashfs not on PATH");
            return;
        }
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
