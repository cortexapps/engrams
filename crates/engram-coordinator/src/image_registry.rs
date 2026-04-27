//! Filesystem-backed image registry.
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   <repo>/                     # e.g. cortex/api → cortex/api/
//!     <tag>/                    # e.g. warm-2026-04-27
//!       manifest.toml           # ImageManifest (required)
//!       rootfs/                 # ProcessBackend: copy this into cwd
//!       rootfs.ext4             # FirecrackerBackend: attach as root drive
//! ```
//!
//! `repo` may contain `/` — it's joined into `<root>` directly. We
//! reject any segment containing `..` so a malicious repo/tag value
//! can't escape `<root>`.
//!
//! # Resolution semantics
//!
//! `load(repo, tag)` returns a `ResolvedImage` if the directory exists
//! and the manifest parses. If the directory doesn't exist, returns
//! `ImageError::NotFound` — callers decide whether to fall through
//! ("no image; empty workdir") or hard-fail.

use std::path::{Path, PathBuf};

use engram_core::types::ImageManifest;

#[derive(Clone, Debug)]
pub struct ImageRegistry {
    root: PathBuf,
}

impl ImageRegistry {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve `<repo>/<tag>` to its on-disk image. Errors if the dir
    /// doesn't exist or the manifest is unparseable. Path traversal
    /// attempts (`..` in either component) are rejected up front so
    /// even a typo in a request body can't escape the registry root.
    pub async fn load(&self, repo: &str, tag: &str) -> Result<ResolvedImage, ImageError> {
        for component in [repo, tag] {
            if component.is_empty() || component.contains("..") || component.starts_with('/') {
                return Err(ImageError::InvalidName(component.to_string()));
            }
        }

        let image_dir = self.root.join(repo).join(tag);
        let manifest_path = image_dir.join("manifest.toml");

        let manifest_bytes = match tokio::fs::read(&manifest_path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ImageError::NotFound {
                    repo: repo.to_string(),
                    tag: tag.to_string(),
                });
            }
            Err(e) => return Err(ImageError::Io(e)),
        };

        let manifest_str = std::str::from_utf8(&manifest_bytes)
            .map_err(|e| ImageError::InvalidManifest(e.to_string()))?;
        let manifest: ImageManifest =
            toml::from_str(manifest_str).map_err(|e| ImageError::InvalidManifest(e.to_string()))?;

        // Pick whichever rootfs flavor exists. Both can coexist (a
        // production image can ship both for dual-platform development),
        // but only one is needed.
        let rootfs_dir = image_dir.join("rootfs");
        let rootfs_ext4 = image_dir.join("rootfs.ext4");
        let rootfs_dir_present = tokio::fs::try_exists(&rootfs_dir).await.unwrap_or(false);
        let rootfs_ext4_present = tokio::fs::try_exists(&rootfs_ext4).await.unwrap_or(false);

        let rootfs = if rootfs_dir_present {
            Rootfs::Directory(rootfs_dir)
        } else if rootfs_ext4_present {
            Rootfs::Ext4Image(rootfs_ext4)
        } else {
            // Manifest-only image — no filesystem materialization. Used
            // for "all I need is the env + secrets" cases.
            Rootfs::None
        };

        Ok(ResolvedImage {
            manifest,
            rootfs,
            image_dir,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedImage {
    pub manifest: ImageManifest,
    pub rootfs: Rootfs,
    /// Top-level directory the manifest lives in, for diagnostics.
    pub image_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub enum Rootfs {
    /// Directory tree to be materialized into the sandbox cwd
    /// (ProcessBackend dev path).
    Directory(PathBuf),
    /// Block-device image to attach as the root drive
    /// (FirecrackerBackend production path).
    Ext4Image(PathBuf),
    /// Manifest-only image: no filesystem to copy/attach. The sandbox
    /// is created with an empty cwd and just gets the env/secrets the
    /// manifest declares.
    None,
}

#[derive(Debug)]
pub enum ImageError {
    NotFound { repo: String, tag: String },
    InvalidName(String),
    InvalidManifest(String),
    Io(std::io::Error),
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { repo, tag } => write!(f, "image not found: {repo}/{tag}"),
            Self::InvalidName(s) => write!(f, "invalid image path component: {s:?}"),
            Self::InvalidManifest(msg) => write!(f, "manifest parse error: {msg}"),
            Self::Io(e) => write!(f, "image registry io: {e}"),
        }
    }
}

impl std::error::Error for ImageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ImageError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn write_manifest(dir: &Path, src: &str) {
        tokio::fs::create_dir_all(dir).await.unwrap();
        tokio::fs::write(dir.join("manifest.toml"), src)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn load_returns_not_found_for_missing_image() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = ImageRegistry::new(tmp.path());
        match reg.load("cortex/api", "missing").await {
            Err(ImageError::NotFound { repo, tag }) => {
                assert_eq!(repo, "cortex/api");
                assert_eq!(tag, "missing");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_rejects_path_traversal_in_repo_or_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = ImageRegistry::new(tmp.path());
        for (repo, tag) in [
            ("..", "x"),
            ("good", ".."),
            ("a/../b", "x"),
            ("/etc", "passwd"),
        ] {
            assert!(
                matches!(reg.load(repo, tag).await, Err(ImageError::InvalidName(_))),
                "{repo}/{tag} must be rejected",
            );
        }
    }

    #[tokio::test]
    async fn load_returns_directory_rootfs_when_dir_present() {
        let tmp = tempfile::tempdir().unwrap();
        let img_dir = tmp.path().join("cortex/api/warm-1");
        write_manifest(&img_dir, r#"name = "cortex-api""#).await;
        tokio::fs::create_dir_all(img_dir.join("rootfs"))
            .await
            .unwrap();

        let reg = ImageRegistry::new(tmp.path());
        let resolved = reg.load("cortex/api", "warm-1").await.unwrap();
        assert_eq!(resolved.manifest.name, "cortex-api");
        match resolved.rootfs {
            Rootfs::Directory(p) => assert!(p.ends_with("rootfs")),
            other => panic!("expected Directory rootfs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_returns_ext4_rootfs_when_only_ext4_present() {
        let tmp = tempfile::tempdir().unwrap();
        let img_dir = tmp.path().join("cortex/api/warm-prod");
        write_manifest(&img_dir, r#"name = "cortex-api""#).await;
        // Pretend rootfs.ext4 file exists.
        tokio::fs::write(img_dir.join("rootfs.ext4"), b"")
            .await
            .unwrap();

        let reg = ImageRegistry::new(tmp.path());
        let resolved = reg.load("cortex/api", "warm-prod").await.unwrap();
        match resolved.rootfs {
            Rootfs::Ext4Image(p) => assert!(p.ends_with("rootfs.ext4")),
            other => panic!("expected Ext4Image rootfs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_returns_none_rootfs_for_manifest_only_image() {
        // Some images are pure declarations — env + secrets, no
        // filesystem. Useful for "give me a session that can talk to
        // GitHub" without bothering with a starter repo.
        let tmp = tempfile::tempdir().unwrap();
        let img_dir = tmp.path().join("plain/x");
        write_manifest(&img_dir, r#"name = "plain""#).await;

        let reg = ImageRegistry::new(tmp.path());
        let resolved = reg.load("plain", "x").await.unwrap();
        assert!(matches!(resolved.rootfs, Rootfs::None));
    }

    #[tokio::test]
    async fn load_surfaces_manifest_parse_errors_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let img_dir = tmp.path().join("broken/x");
        // Missing required `name` field.
        write_manifest(&img_dir, "wat = 'lol'").await;
        let reg = ImageRegistry::new(tmp.path());
        match reg.load("broken", "x").await {
            Err(ImageError::InvalidManifest(msg)) => {
                assert!(!msg.is_empty(), "parse error must carry detail");
            }
            other => panic!("expected InvalidManifest, got {other:?}"),
        }
    }
}
