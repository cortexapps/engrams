//! Pack a directory tree into an ext4 disk image, ready for
//! `FirecrackerBackend` to attach as a root drive.
//!
//! The default [`Mke2fsPacker`] shells out to `mke2fs -t ext4 -F -d`,
//! which (since e2fsprogs 1.43) populates the freshly-formatted
//! filesystem from a source directory in one shot — no loopback
//! mount, no root needed. This is the same flow Firecracker's CI uses
//! to bake their published `ubuntu-*.ext4` artifacts.
//!
//! [`Ext4Packer`] is a trait so the unit tests can mock it; the
//! integration test in `tests/builder.rs` exercises the real binary
//! against a real source dir.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

#[async_trait]
pub trait Ext4Packer: Send + Sync {
    /// Create `dst_image` (overwriting any existing file) of size
    /// `size_bytes`, format it as ext4, and copy the contents of
    /// `src_dir` into the new filesystem. The image is left ready for
    /// Firecracker to attach as a block device.
    async fn pack(
        &self,
        src_dir: &Path,
        dst_image: &Path,
        size_bytes: u64,
    ) -> Result<(), Ext4Error>;
}

#[derive(Debug)]
pub enum Ext4Error {
    Io(std::io::Error),
    /// mke2fs returned a non-zero exit code. The string is its
    /// captured stderr — verbose, but useful when the bake fails.
    Mke2fs(String),
    /// Could not find the mke2fs binary (PATH miss or stale config).
    MissingBinary(String),
}

impl std::fmt::Display for Ext4Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Mke2fs(s) => write!(f, "mke2fs: {s}"),
            Self::MissingBinary(b) => write!(f, "binary not found: {b}"),
        }
    }
}

impl std::error::Error for Ext4Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Ext4Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Production [`Ext4Packer`] backed by `mke2fs` from e2fsprogs.
#[derive(Clone, Debug)]
pub struct Mke2fsPacker {
    bin: PathBuf,
}

impl Default for Mke2fsPacker {
    fn default() -> Self {
        Self {
            bin: PathBuf::from("mke2fs"),
        }
    }
}

impl Mke2fsPacker {
    pub fn with_binary(bin: impl Into<PathBuf>) -> Self {
        Self { bin: bin.into() }
    }
}

#[async_trait]
impl Ext4Packer for Mke2fsPacker {
    async fn pack(
        &self,
        src_dir: &Path,
        dst_image: &Path,
        size_bytes: u64,
    ) -> Result<(), Ext4Error> {
        // Atomic write: format into `<dst>.tmp` and rename on success.
        // On any error path (missing binary, mke2fs failure, IO), the
        // tmp file gets cleaned up so the next attempt starts fresh.
        // Without this, a missing-binary failure leaves the
        // preallocated zero-padded file at `dst_image`, which downstream
        // cache-presence checks happily mistake for a built artifact —
        // the VM then mounts a block of zeros as ext4 and the harness
        // never appears.
        let mut tmp = dst_image.to_path_buf();
        tmp.as_mut_os_string().push(".tmp");

        // 1. Truncate / preallocate. mke2fs reads the file's size to
        //    decide how big to make the filesystem; we want exactly
        //    `size_bytes`.
        let f = tokio::fs::File::create(&tmp).await?;
        f.set_len(size_bytes).await?;
        drop(f);

        // 2. Format + populate in one mke2fs call. Best-effort cleanup
        //    of the tmp file on any failure path; ignore cleanup errors
        //    since the original mke2fs error is what the caller cares
        //    about.
        let result = self.run_mke2fs(src_dir, &tmp).await;
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }

        // 3. Atomic rename. Only after this returns Ok does the cache
        //    presence check on `dst_image` start returning true.
        tokio::fs::rename(&tmp, dst_image).await?;
        Ok(())
    }
}

impl Mke2fsPacker {
    /// Inner mke2fs invocation, factored out so the caller can wrap
    /// the failure path in tmp-file cleanup without duplicating
    /// argument construction.
    async fn run_mke2fs(&self, src_dir: &Path, dst_image: &Path) -> Result<(), Ext4Error> {
        let output = tokio::process::Command::new(&self.bin)
            .arg("-t")
            .arg("ext4")
            .arg("-F")
            .arg("-q")
            .arg("-d")
            .arg(src_dir)
            .arg(dst_image)
            .output()
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ext4Error::MissingBinary(self.bin.to_string_lossy().into_owned())
                } else {
                    Ext4Error::Io(e)
                }
            })?;

        if !output.status.success() {
            let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            if stderr.is_empty() {
                stderr = format!("exit status {}", output.status);
            }
            return Err(Ext4Error::Mke2fs(stderr));
        }
        Ok(())
    }
}

/// Pick a sensible image size for `dir_size_bytes` of source data:
/// ext4 metadata (~3-5%), inode table, journal (~64 MiB by default),
/// plus headroom for the booted VM to write to /tmp etc.
///
/// Formula: `max(dir_size * 2, dir_size + 128 MiB)`, rounded up to
/// a 4 KiB boundary. The doubling catches small images (a 50 MiB
/// rootfs gets 178 MiB, plenty for the journal); the +128 MiB
/// minimum catches tiny test images where 2× doesn't even cover
/// ext4's overhead. The 4 KiB rounding is required by macOS Tahoe's
/// VZ disk-image attachment, which rejects files that aren't a
/// multiple of the 512-byte sector size with `VZErrorDomain code=5
/// "Invalid disk image"`. We pick 4 KiB instead of 512 to match
/// ext4's default block size — same alignment as a real block
/// device.
pub fn recommended_size(dir_size_bytes: u64) -> u64 {
    let twice = dir_size_bytes.saturating_mul(2);
    let plus_128 = dir_size_bytes.saturating_add(128 * 1024 * 1024);
    let raw = twice.max(plus_128);
    const ALIGN: u64 = 4096;
    raw.saturating_add(ALIGN - 1) & !(ALIGN - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommended_size_uses_2x_for_large_images() {
        // 500 MiB dir → 1 GiB image (2x dominates over +128 MiB).
        let s = 500 * 1024 * 1024;
        assert_eq!(recommended_size(s), 2 * s);
    }

    #[test]
    fn recommended_size_uses_plus_128mib_for_small_images() {
        // 10 MiB dir → 138 MiB image (+128 MiB dominates over 2x = 20 MiB).
        let s = 10 * 1024 * 1024;
        assert_eq!(recommended_size(s), s + 128 * 1024 * 1024);
    }

    #[test]
    fn recommended_size_handles_zero() {
        // Empty dir still gets a 128 MiB image rather than 0.
        assert_eq!(recommended_size(0), 128 * 1024 * 1024);
    }

    #[test]
    fn recommended_size_does_not_overflow() {
        // Adversarial input doesn't panic.
        assert!(recommended_size(u64::MAX) > 0);
    }

    #[test]
    fn recommended_size_is_aligned_to_4kib() {
        // Awkward source sizes that previously produced a non-sector-
        // aligned image. 897_419_577 is the exact dir size the Claude
        // bake hit in the field; before this fix 2× was 1_794_839_154,
        // 114 bytes shy of sector alignment, and macOS Tahoe's VZ
        // refused to attach it ("Invalid disk image. The disk image
        // format is not recognized.").
        for s in [
            1u64,
            511,
            512,
            897_419_577,
            (1 << 30) + 1,
            (10 * 1024 * 1024) + 7,
        ] {
            assert_eq!(
                recommended_size(s) % 4096,
                0,
                "recommended_size({s}) must be 4 KiB aligned for VZ"
            );
        }
    }
}
