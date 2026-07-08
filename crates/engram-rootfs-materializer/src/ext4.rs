//! Pack a directory tree into an ext4 disk image, ready for
//! `FirecrackerBackend` to attach as a root drive.
//!
//! The default [`Mke2fsPacker`] shells out to `mke2fs -t ext4 -F -d`,
//! which (since e2fsprogs 1.43) populates the freshly-formatted
//! filesystem from a source directory in one shot — no loopback
//! mount, no root needed. This is the same flow Firecracker's CI uses
//! to bake their published `ubuntu-*.ext4` artifacts.
//!
//! ADR 0080: this module is the single home for tree → ext4 packing,
//! whether the tree came from `docker export` (the retiring bake) or
//! an OCI-layer flatten (this crate).
//!
//! [`Ext4Packer`] is a trait so unit tests can mock it; the real
//! binary is exercised by the packer/determinism integration tests.

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
            bin: resolve_mke2fs(),
        }
    }
}

impl Mke2fsPacker {
    pub fn with_binary(bin: impl Into<PathBuf>) -> Self {
        Self { bin: bin.into() }
    }
}

/// Resolve the `mke2fs` to shell out to, preferring a pinned one.
///
/// ADR 0036 byte-determinism requires an e2fsprogs that honors
/// `SOURCE_DATE_EPOCH` (>= 1.47.1); most distros' system e2fsprogs is older and
/// silently stamps wall-clock times, breaking cross-bake chunk dedup. So we
/// don't rely on whatever `mke2fs` happens to be on `$PATH` — we ship a pinned
/// static `mke2fs` *next to the current executable* (the `cli-tools` artifact
/// for bakes; ADR 0080 moves the same pin into the host-agent image for
/// enable-time materialization) and resolve it here, so the pack is
/// deterministic by construction wherever it runs. Order:
///
/// 1. `$ENGRAM_MKE2FS` — explicit override (CI's packer test, debugging).
/// 2. an `mke2fs` sibling of the current executable — the bundled pin.
/// 3. `mke2fs` from `$PATH` — `nix develop` dev shells, and hosts whose
///    distro e2fsprogs packs only content where determinism is immaterial
///    (e.g. the empty stub harness).
fn resolve_mke2fs() -> PathBuf {
    if let Some(p) = std::env::var_os("ENGRAM_MKE2FS") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(sibling) = exe.parent().map(|d| d.join("mke2fs")) {
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("mke2fs")
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

/// ADR 0036: fixed inputs that make `mke2fs` output a pure function
/// of the source tree, so a re-bake with unchanged content reproduces
/// identical 16 MiB chunk hashes — the property that turns the
/// content-addressed chunk store and the per-chunk OCI push into
/// *delta* transfers. Without these, mke2fs salts every image with a
/// random filesystem UUID, a random htree directory-hash seed, and
/// wall-clock timestamps, and cross-bake dedup never fires (~100% new
/// chunks per re-bake of 95%-identical content).
///
/// Sharing one filesystem UUID across all engram images is safe:
/// guests mount the rootfs by virtio device path, never by UUID, and
/// the images are block devices inside dedicated microVMs (no host
/// blkid involvement).
const DETERMINISTIC_FS_UUID: &str = "00000000-e9a4-4a11-8036-000000000036";
const DETERMINISTIC_HASH_SEED: &str = "00000000-5eed-4a11-8036-000000000036";
/// 2024-01-01T00:00:00Z. e2fsprogs (≥1.45) reads `SOURCE_DATE_EPOCH`
/// and (a) stamps superblock mkfs/write times from it instead of the
/// wall clock, and (b) clamps inode timestamps newer than it — which
/// covers files injected at materialize time (the init shim, whiteout
/// side effects). [`clamp_mtimes`] performs the same clamp in the tree
/// itself as belt-and-braces (the reproducible-bundle lesson: one path
/// skipping the clamp produced sha mismatches).
/// Verified empirically: same tree packed twice (and two
/// separately-created identical trees) → byte-identical images.
pub const DETERMINISTIC_EPOCH_SECS: u64 = 1_704_067_200;
const DETERMINISTIC_EPOCH: &str = "1704067200";

impl Mke2fsPacker {
    /// Inner mke2fs invocation, factored out so the caller can wrap
    /// the failure path in tmp-file cleanup without duplicating
    /// argument construction.
    async fn run_mke2fs(&self, src_dir: &Path, dst_image: &Path) -> Result<(), Ext4Error> {
        // Provision the inode table from the actual entry count, not
        // mke2fs's default (size / 16 KiB). A tree of many tiny files —
        // node_modules, gradle/pnpm caches — exhausts the default inode
        // count long before it runs out of blocks (`mke2fs: No space left
        // on device while populating file system`, even with the 2x size
        // headroom). See `recommended_inodes` — but cap it to what THIS
        // filesystem can hold (the dst image is preallocated to its final
        // size above): `recommended_inodes` floors at ~131072 for the
        // multi-GiB image case, which a small fs (the 16 MiB host stub
        // harness) can't fit, and mke2fs hard-rejects an oversized inode
        // table ("inode_size * inodes_count too big for a filesystem with
        // N blocks").
        let fs_size_bytes = tokio::fs::metadata(dst_image)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let num_inodes = inode_count_for(count_entries(src_dir).await, fs_size_bytes);
        let output = tokio::process::Command::new(&self.bin)
            .arg("-t")
            .arg("ext4")
            .arg("-F")
            .arg("-q")
            // ADR 0036 determinism: fixed FS UUID + directory-hash
            // seed + epoch (see the consts above).
            .arg("-U")
            .arg(DETERMINISTIC_FS_UUID)
            .arg("-E")
            .arg(format!("hash_seed={DETERMINISTIC_HASH_SEED}"))
            // Explicit inode count (deterministic: derived from the entry
            // count, quantized — see recommended_inodes).
            .arg("-N")
            .arg(num_inodes.to_string())
            .env("SOURCE_DATE_EPOCH", DETERMINISTIC_EPOCH)
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
    // ADR 0036: quantize COARSELY so the filesystem geometry is
    // stable under small source-tree drift. mke2fs derives block
    // groups, bitmaps, and inode-table placement from the total
    // block count — with fine-grained (4 KiB) rounding, an 8 KB
    // change in a multi-GB tree changes the fs size, which shifts
    // every structure on the disk and re-rolls ~100% of the 16 MiB
    // chunk hashes (observed in prod: two bakes of the same source
    // deduped 1% purely from apt-index churn). Snapping to 256 MiB
    // bands keeps geometry identical until the tree grows past a
    // band edge; the padding is zero-filled and zero chunks are
    // elided from manifests, so the cost is a few extra inode-table
    // chunks, not storage. Small images (< 1 GiB raw) keep 4 KiB
    // alignment — dev/test fixtures stay tight, and VZ's
    // sector-alignment requirement is satisfied by both branches.
    const FINE: u64 = 4096;
    const COARSE: u64 = 256 * 1024 * 1024;
    let align = if raw > 1024 * 1024 * 1024 {
        COARSE
    } else {
        FINE
    };
    raw.saturating_add(align - 1) & !(align - 1)
}

/// Count filesystem entries (regular files, dirs, symlinks — one inode
/// each) under `dir`. Does NOT follow symlinks: `mke2fs -d` replicates a
/// symlink as a symlink (one inode), and not following also avoids walking
/// symlink farms (pnpm `node_modules`) or cycles. Used to size the inode
/// table via [`recommended_inodes`].
async fn count_entries(dir: &Path) -> u64 {
    let mut n = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut rd = match tokio::fs::read_dir(&d).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            n = n.saturating_add(1);
            // `file_type()` reflects the entry itself (readdir d_type),
            // NOT the symlink target — so we only descend into real dirs.
            if let Ok(ft) = entry.file_type().await {
                if ft.is_dir() {
                    stack.push(entry.path());
                }
            }
        }
    }
    n
}

/// Inodes to provision for a tree of `entry_count` entries. mke2fs's
/// default inode count is `fs_size / 16 KiB`, which a tree of many tiny
/// files (node_modules, gradle/pnpm caches) blows past — it runs out of
/// inodes long before blocks. Size from the real entry count instead:
/// the entries + 50% headroom (root-fs writes at runtime — /tmp, logs,
/// the warm gradle daemon's scratch) + a floor, then quantized to a
/// coarse band so small source-tree drift doesn't re-roll the fs geometry
/// (same determinism rationale as [`recommended_size`]'s COARSE banding).
pub fn recommended_inodes(entry_count: u64) -> u64 {
    let raw = entry_count.saturating_mul(3) / 2 + 100_000;
    // 128 Ki bands. An inode is 256 B, so a band is ~32 MiB of inode
    // table — negligible against the multi-GiB images this matters for,
    // and it keeps `-N` stable until the entry count crosses a band edge.
    const BAND: u64 = 128 * 1024;
    raw.saturating_add(BAND - 1) & !(BAND - 1)
}

/// The mke2fs `-N` inode count for an ext4 of `fs_size_bytes` holding
/// `entry_count` files: the entry-driven [`recommended_inodes`] target,
/// CAPPED to what the filesystem can physically hold.
///
/// `recommended_inodes` floors at ~131072 (the 128 Ki band) for the
/// multi-GiB image case, where a 32 MiB inode table is noise. A small fs
/// can't hold that: an inode is 256 B, and mke2fs's hard ceiling is
/// `fs_size / inode_size` (all blocks as inode table) — e.g. the 16 MiB
/// host stub harness tops out at 65536 inodes and mke2fs rejects the
/// 131072 floor outright. We reserve ≤ 1/4 of the fs for inodes so blocks
/// + journal + metadata still fit, never dropping below a small floor.
pub fn inode_count_for(entry_count: u64, fs_size_bytes: u64) -> u64 {
    const INODE_SIZE_B: u64 = 256;
    let recommended = recommended_inodes(entry_count);
    if fs_size_bytes == 0 {
        return recommended; // size unknown — leave the recommendation as-is.
    }
    let cap = (fs_size_bytes / (INODE_SIZE_B * 4)).max(16);
    recommended.min(cap)
}

/// Sum the *actual disk usage* (allocated 512-byte blocks, à la `du`) of every
/// entry under `dir`, without following symlinks.
///
/// We size from `st_blocks`, NOT apparent file length (`meta.len()`): the
/// brain/gradle/pnpm caches baked into warm dev images are hundreds of
/// thousands of tiny files, and ext4 rounds every file up to a 4 KiB block
/// (plus a block per directory). Summing apparent lengths undercounts real
/// block consumption by 2-4× for such trees, so `recommended_size`'s 2×
/// headroom still undershot and `mke2fs -d` hit ENOSPC mid-populate (the
/// dev-brain bake). Block usage captures the rounding, directory blocks, and
/// xattr/inline overhead directly.
///
/// Counting is per-entry, so a hardlink (pnpm's content-addressed store links
/// into `node_modules`) is counted once per link — an overcount, but in the
/// safe direction (a slightly larger fs is fine; a too-small one is fatal).
/// Not following symlinks matches `count_entries` and `mke2fs -d`, which
/// replicates a symlink as a symlink: we count the link inode's own blocks and
/// reach a target only if it lives in the real tree. `symlink_metadata`
/// (lstat) also can't error on broken symlinks, unlike `metadata`.
pub async fn recursive_size(dir: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&d).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        while let Some(entry) = entries.next_entry().await? {
            // `file_type()` is readdir's d_type — it reflects the entry
            // itself, not a symlink target, so we descend only real dirs.
            let ft = entry.file_type().await?;
            // lstat: count the entry's own allocated blocks (st_blocks is in
            // 512-byte units), never the symlink target.
            let meta = tokio::fs::symlink_metadata(entry.path()).await?;
            total = total.saturating_add(meta.blocks().saturating_mul(512));
            if ft.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(total)
}

/// Clamp every mtime under `root` (files, dirs, symlinks) to
/// [`DETERMINISTIC_EPOCH_SECS`] — anything newer is set to the epoch;
/// older (tar-carried, already deterministic) timestamps are left
/// alone. Belt-and-braces for the pack's `SOURCE_DATE_EPOCH` clamp:
/// e2fsprogs < 1.47.1 silently ignores the env var, and the
/// reproducible-bundle incident taught us that any single path
/// skipping the clamp shows up later as a sha-mismatch head-scratcher.
/// Run right before [`Ext4Packer::pack`]. Blocking — call from
/// `spawn_blocking`.
pub fn clamp_mtimes(root: &Path) -> std::io::Result<()> {
    let clamp = filetime::FileTime::from_unix_time(DETERMINISTIC_EPOCH_SECS as i64, 0);
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        // Clamp the directory itself AFTER queueing (children writes
        // won't touch it again — we only read below this point).
        for entry in std::fs::read_dir(&d)? {
            let entry = entry?;
            let p = entry.path();
            let meta = std::fs::symlink_metadata(&p)?;
            if meta.is_dir() {
                stack.push(p.clone());
            }
            let mtime = filetime::FileTime::from_last_modification_time(&meta);
            if mtime > clamp {
                // lutimes: never follow symlinks (the target may not
                // even exist inside the tree).
                filetime::set_symlink_file_times(&p, clamp, clamp)?;
            }
        }
        let meta = std::fs::symlink_metadata(&d)?;
        if filetime::FileTime::from_last_modification_time(&meta) > clamp {
            filetime::set_symlink_file_times(&d, clamp, clamp)?;
        }
    }
    Ok(())
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
    fn recommended_inodes_covers_count_plus_headroom_and_quantizes() {
        const BAND: u64 = 128 * 1024;
        // Always a multiple of the band, and strictly above the entry
        // count (so every entry gets an inode, with headroom to spare).
        for count in [0u64, 1, 50_000, 300_000, 1_000_000, 5_000_000] {
            let n = recommended_inodes(count);
            assert_eq!(n % BAND, 0, "must be band-aligned for count {count}");
            assert!(n > count, "{n} inodes must exceed {count} entries");
            assert!(n >= count + count / 2, "must include ~50% headroom");
        }
        // Small trees still get a usable floor (~100k → 128 Ki band).
        assert_eq!(recommended_inodes(0), BAND);
        // A high-file-count tree (e.g. ~1M node_modules/gradle entries)
        // gets ~1.5M inodes — far above mke2fs's default of size/16KiB.
        assert!(recommended_inodes(1_000_000) >= 1_500_000);
    }

    #[test]
    fn inode_count_for_caps_to_filesystem_capacity() {
        const MIB: u64 = 1024 * 1024;
        const INODE_SIZE_B: u64 = 256;
        // The 16 MiB host stub harness: the recommended floor (131072)
        // overruns mke2fs's fs_size/inode_size ceiling (65536) and is
        // rejected. inode_count_for must clamp below the floor AND below
        // the ceiling, leaving room for blocks.
        let n = inode_count_for(0, 16 * MIB);
        assert!(
            n < recommended_inodes(0),
            "tiny fs must clamp below the floor"
        );
        assert!(
            n <= 16 * MIB / INODE_SIZE_B,
            "must fit mke2fs's hard ceiling"
        );
        assert!(n >= 16, "still a usable minimum");
        // A multi-GiB image: the cap doesn't bite, the recommendation wins.
        assert_eq!(
            inode_count_for(1_000_000, 8192 * MIB),
            recommended_inodes(1_000_000),
        );
        // Unknown size (0) leaves the recommendation untouched.
        assert_eq!(inode_count_for(50_000, 0), recommended_inodes(50_000));
    }

    #[test]
    fn recommended_size_does_not_overflow() {
        // Adversarial input doesn't panic.
        assert!(recommended_size(u64::MAX) > 0);
    }

    /// ADR 0036: sizes past 1 GiB snap to 256 MiB bands, so small
    /// source-tree drift (the apt-churn class) maps to the SAME
    /// filesystem geometry and cross-bake chunk dedup survives.
    #[test]
    fn recommended_size_quantizes_large_images_to_256mib_bands() {
        const MIB: u64 = 1024 * 1024;
        let a = recommended_size(4800 * MIB);
        let b = recommended_size(4800 * MIB + 8 * 1024); // +8 KB tree drift
        assert_eq!(a, b, "small drift must not change the fs size");
        assert_eq!(a % (256 * MIB), 0, "large sizes snap to 256 MiB");
        // Still never smaller than the raw requirement.
        assert!(a >= 2 * 4800 * MIB);
        // And a genuinely larger tree eventually crosses a band edge.
        assert!(recommended_size(5200 * MIB) > a);
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

    /// The determinism clamp: newer-than-epoch mtimes (freshly written
    /// files) snap to the epoch; older, tar-carried mtimes survive.
    /// Symlinks are clamped via lutimes (never following the target).
    #[test]
    fn clamp_mtimes_clamps_new_and_keeps_old() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/fresh"), b"now").unwrap(); // wall clock, > epoch
        std::fs::write(root.join("old"), b"then").unwrap();
        let old = filetime::FileTime::from_unix_time(1_000_000, 0); // 1970s
        filetime::set_file_mtime(root.join("old"), old).unwrap();
        std::os::unix::fs::symlink("missing-target", root.join("dangling")).unwrap();

        clamp_mtimes(root).unwrap();

        let mt = |p: &str| {
            let m = std::fs::symlink_metadata(root.join(p)).unwrap();
            filetime::FileTime::from_last_modification_time(&m).unix_seconds()
        };
        assert_eq!(mt("sub/fresh"), DETERMINISTIC_EPOCH_SECS as i64);
        assert_eq!(mt("sub"), DETERMINISTIC_EPOCH_SECS as i64);
        assert_eq!(mt("old"), 1_000_000, "pre-epoch mtimes are preserved");
        assert_eq!(
            mt("dangling"),
            DETERMINISTIC_EPOCH_SECS as i64,
            "symlink itself is clamped without following its target"
        );
    }
}
