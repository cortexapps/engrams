//! Shared `mksquashfs` invocation for content-addressed read-only bundles.
//!
//! Both the ADR 0055 uploaded-skill packer ([`crate::skill_pack`]) and the ADR
//! 0062 custom-harness registration ([`crate::harness_catalog`]) stage a tree and
//! pack it through here, so identical content yields an identical sha256 (the
//! content address `bundles/sha256/<sha>`) regardless of which producer built it.

use std::path::Path;
use std::process::Command;

/// Pack `tree` into a squashfs under reproducible flags and return its bytes.
///
/// Determinism (identical tree → identical sha, so re-staging is idempotent):
/// `-all-root` (root-owned, as the guest mounts RO), `-no-xattrs`, `-comp zstd`
/// (matching the `deploy/bundles/*/build.sh` recipes), and a pinned
/// `SOURCE_DATE_EPOCH` so mksquashfs clamps every timestamp to a fixed value
/// (the prod coordinator container has no ambient epoch; the nix dev shell sets
/// its own, so we override to a constant either way). We must NOT also pass
/// `-mkfs-time`/`-all-time` — mksquashfs refuses both at once.
pub(crate) fn pack_dir(tree: &Path) -> Result<Vec<u8>, String> {
    let out_dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let out_path = out_dir.path().join("bundle.squashfs");
    let output = Command::new("mksquashfs")
        .arg(tree)
        .arg(&out_path)
        .args(["-comp", "zstd", "-all-root", "-noappend", "-no-xattrs"])
        .env("SOURCE_DATE_EPOCH", "0")
        .output()
        .map_err(|e| format!("spawn mksquashfs (is squashfs-tools installed?): {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "mksquashfs exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    std::fs::read(&out_path).map_err(|e| format!("read squashfs: {e}"))
}
