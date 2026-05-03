//! Read-only ext4 image of `cfg.harnesses_dir` that every sandbox
//! attaches as its second virtio-blk drive (`/dev/vdb`).
//!
//! Background: harness binaries (`engram-harness-noop`,
//! `engram-harness-claude`, …) live host-side under the deployment's
//! `cfg.harnesses_dir`, not inside the image. VZ used to expose them
//! via virtio-fs; mainline Firecracker has no virtio-fs device, and
//! the user's design constraint is that VZ should exercise the same
//! production path as FC. The compromise: pack the dir into a tiny
//! read-only ext4 image and attach it as a second virtio-blk drive on
//! both backends. Trade-off vs. virtio-fs: changes to
//! `cfg.harnesses_dir` don't propagate to live sandboxes — operators
//! who add a harness restart the coord (or hit a future
//! `POST /admin/refresh-harnesses` endpoint).
//!
//! The substrate is built once at coordinator startup and rebuilt on
//! demand when a content hash mismatches.

use std::path::{Path, PathBuf};

use engram_image_builder::{recommended_size, Ext4Packer, Mke2fsPacker};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct Substrate {
    /// Absolute path to `<work_dir>/harness-substrate.img`. Backends
    /// receive this via `SandboxSpec.harness_substrate` and attach it
    /// as a read-only virtio-blk drive.
    pub path: PathBuf,
    /// SHA-256 of the *contents* of `cfg.harnesses_dir` at build
    /// time. Used by `refresh()` to detect drift, and stamped into
    /// snapshot manifests so a future strict-restore mode can refuse
    /// to resume against a substrate that's drifted out from under
    /// it.
    pub hash: String,
}

#[derive(Debug)]
pub enum SubstrateError {
    Io(std::io::Error),
    Pack(engram_image_builder::Ext4Error),
}

impl std::fmt::Display for SubstrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Pack(e) => write!(f, "ext4 pack: {e}"),
        }
    }
}

impl std::error::Error for SubstrateError {}

impl From<std::io::Error> for SubstrateError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<engram_image_builder::Ext4Error> for SubstrateError {
    fn from(e: engram_image_builder::Ext4Error) -> Self {
        Self::Pack(e)
    }
}

/// Pack `harnesses_dir` into `<work_dir>/harness-substrate.img` and
/// return a [`Substrate`] handle. If `harnesses_dir` is empty or
/// missing, returns Ok(None) (sandboxes still boot, just without
/// `/run/engram/harnesses`).
///
/// `ca_pem`, if Some, gets stamped into the substrate at
/// `/.engram-host/ca.pem`. The init shim copies it into the guest
/// trust store at boot so MITM leaves signed by the egress proxy
/// validate.
pub async fn build(
    harnesses_dir: &Path,
    work_dir: &Path,
    ca_pem: Option<&str>,
) -> Result<Option<Substrate>, SubstrateError> {
    if !harnesses_dir.exists() && ca_pem.is_none() {
        return Ok(None);
    }
    let dir_hash = hash_dir(harnesses_dir).await?;
    let ca_hash = ca_pem.map(blake_lite).unwrap_or_default();
    if dir_hash.is_empty() && ca_hash.is_empty() {
        return Ok(None);
    }
    let hash = format!("{dir_hash}-{ca_hash}");

    tokio::fs::create_dir_all(work_dir).await?;
    let image_path = work_dir.join("harness-substrate.img");

    // Stage a tempdir we can pack: copy the harnesses tree (if
    // present) and write the CA at .engram-host/ca.pem (if
    // present). The packer ignores the stage dir's own metadata —
    // it just walks the tree.
    let stage = work_dir.join("stage");
    if stage.exists() {
        tokio::fs::remove_dir_all(&stage).await?;
    }
    tokio::fs::create_dir_all(&stage).await?;
    if harnesses_dir.exists() {
        copy_dir(harnesses_dir, &stage).await?;
    }
    if let Some(pem) = ca_pem {
        let host_meta = stage.join(".engram-host");
        tokio::fs::create_dir_all(&host_meta).await?;
        tokio::fs::write(host_meta.join("ca.pem"), pem).await?;
    }

    let dir_size = recursive_size(&stage).await?;
    if dir_size == 0 {
        let _ = tokio::fs::remove_dir_all(&stage).await;
        return Ok(None);
    }
    let size = recommended_size(dir_size);

    let packer = Mke2fsPacker::default();
    let pack_result = packer.pack(&stage, &image_path, size).await;
    let _ = tokio::fs::remove_dir_all(&stage).await;
    pack_result?;

    Ok(Some(Substrate {
        path: image_path,
        hash,
    }))
}

/// SHA-256 of a string, hex-encoded. Used to mix the CA PEM into
/// the substrate hash so a CA rotation forces a substrate rebuild.
fn blake_lite(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

async fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((from, to)) = stack.pop() {
        tokio::fs::create_dir_all(&to).await?;
        let mut rd = tokio::fs::read_dir(&from).await?;
        while let Some(ent) = rd.next_entry().await? {
            let ft = ent.file_type().await?;
            let dst_path = to.join(ent.file_name());
            if ft.is_dir() {
                stack.push((ent.path(), dst_path));
            } else if ft.is_file() {
                tokio::fs::copy(ent.path(), &dst_path).await?;
                // Preserve executable bit so harness binaries stay
                // executable inside the guest.
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = tokio::fs::metadata(ent.path()).await {
                    let mode = meta.permissions().mode();
                    let _ = tokio::fs::set_permissions(
                        &dst_path,
                        std::fs::Permissions::from_mode(mode),
                    )
                    .await;
                }
            }
        }
    }
    Ok(())
}

/// Re-hash `harnesses_dir` (+ optional CA) and rebuild only if the
/// hash differs from `current.hash`. Returns `Ok(None)` if unchanged.
pub async fn refresh(
    harnesses_dir: &Path,
    work_dir: &Path,
    ca_pem: Option<&str>,
    current: &Substrate,
) -> Result<Option<Substrate>, SubstrateError> {
    let dir_hash = hash_dir(harnesses_dir).await?;
    let ca_hash = ca_pem.map(blake_lite).unwrap_or_default();
    let new_hash = format!("{dir_hash}-{ca_hash}");
    if new_hash == current.hash {
        return Ok(None);
    }
    build(harnesses_dir, work_dir, ca_pem).await
}

/// Walk `dir` deterministically (sorted) and return `sha256("name\0size\0contents…")`
/// over every regular file. Returns `""` if the dir is empty or doesn't exist.
async fn hash_dir(dir: &Path) -> std::io::Result<String> {
    let mut entries = collect_files(dir).await?;
    if entries.is_empty() {
        return Ok(String::new());
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (rel, full) in entries {
        let bytes = tokio::fs::read(&full).await?;
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn collect_files(root: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        while let Some(ent) = rd.next_entry().await? {
            let path = ent.path();
            let ft = ent.file_type().await?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                out.push((rel, path));
            }
        }
    }
    Ok(out)
}

async fn recursive_size(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    let files = collect_files(dir).await?;
    for (_, p) in files {
        if let Ok(meta) = tokio::fs::metadata(&p).await {
            total += meta.len();
        }
    }
    Ok(total)
}

impl Substrate {
    /// `true` when there's no usable substrate to attach (empty
    /// directory, build failed gracefully, or operator hasn't run
    /// `just install-harnesses` yet).
    pub fn is_empty(&self) -> bool {
        self.hash.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hash_dir_is_stable_for_same_content() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(tmp.path().join("a")).await.unwrap();
        tokio::fs::write(tmp.path().join("a/x"), b"hello").await.unwrap();
        tokio::fs::write(tmp.path().join("y"), b"world").await.unwrap();

        let h1 = hash_dir(tmp.path()).await.unwrap();
        let h2 = hash_dir(tmp.path()).await.unwrap();
        assert_eq!(h1, h2);
        assert!(!h1.is_empty());
    }

    #[tokio::test]
    async fn hash_dir_changes_with_content() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("x"), b"v1").await.unwrap();
        let h1 = hash_dir(tmp.path()).await.unwrap();
        tokio::fs::write(tmp.path().join("x"), b"v2").await.unwrap();
        let h2 = hash_dir(tmp.path()).await.unwrap();
        assert_ne!(h1, h2);
    }

    #[tokio::test]
    async fn hash_dir_empty_returns_empty_string() {
        let tmp = tempfile::tempdir().unwrap();
        let h = hash_dir(tmp.path()).await.unwrap();
        assert!(h.is_empty());
    }

    #[tokio::test]
    async fn build_returns_none_for_empty_dir_and_no_ca() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let s = build(tmp.path(), work.path(), None).await.unwrap();
        assert!(s.is_none());
    }

    #[tokio::test]
    async fn build_returns_none_for_missing_dir_and_no_ca() {
        let work = tempfile::tempdir().unwrap();
        let s = build(Path::new("/nonexistent/engram-harnesses"), work.path(), None)
            .await
            .unwrap();
        assert!(s.is_none());
    }

    #[test]
    fn ca_hash_changes_substrate_hash() {
        let h1 = blake_lite("-----BEGIN CERTIFICATE-----\nA\n-----END CERTIFICATE-----\n");
        let h2 = blake_lite("-----BEGIN CERTIFICATE-----\nB\n-----END CERTIFICATE-----\n");
        assert_ne!(h1, h2);
        let h3 = blake_lite("-----BEGIN CERTIFICATE-----\nA\n-----END CERTIFICATE-----\n");
        assert_eq!(h1, h3);
    }
}
