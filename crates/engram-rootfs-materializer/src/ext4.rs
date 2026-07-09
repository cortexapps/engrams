//! Pack a flattened rootfs tar into an ext4 disk image, ready for
//! `FirecrackerBackend` to attach as a root drive.
//!
//! ADR 0084: the host scratch tree stores contents only. We first emit
//! one deterministic tar whose headers carry the OCI uid/gid/mode/mtime
//! and xattr metadata, then the default [`Mke2fsPacker`] shells out to
//! `mke2fs -t ext4 -F -d <tar>`. Metadata flows as tar data, never
//! through host inode ownership.
//!
//! ADR 0080: this module is the single home for tree → ext4 packing,
//! whether the tree came from `docker export` (the retiring bake) or
//! an OCI-layer flatten (this crate).
//!
//! [`Ext4Packer`] is a trait so unit tests can mock it; the real
//! binary is exercised by the packer/determinism integration tests.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::BufReader;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::flatten::{SkippedSpecialKind, TreeMetadata};

#[async_trait]
pub trait Ext4Packer: Send + Sync {
    /// Create `dst_image` (overwriting any existing file) of size
    /// `size_bytes`, format it as ext4, and copy the contents of
    /// `src_tar` into the new filesystem. The image is left ready for
    /// Firecracker to attach as a block device.
    async fn pack(
        &self,
        src_tar: &Path,
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
        src_tar: &Path,
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
        let result = self.run_mke2fs(src_tar, &tmp).await;
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
/// side effects). [`emit_tar`] performs the same clamp in the emitted
/// tar itself as belt-and-braces (the reproducible-bundle lesson: one
/// path skipping the clamp produced sha mismatches).
/// Verified empirically: same tree packed twice (and two
/// separately-created identical trees) → byte-identical images.
pub const DETERMINISTIC_EPOCH_SECS: u64 = 1_704_067_200;
const DETERMINISTIC_EPOCH: &str = "1704067200";

impl Mke2fsPacker {
    /// Inner mke2fs invocation, factored out so the caller can wrap
    /// the failure path in tmp-file cleanup without duplicating
    /// argument construction.
    async fn run_mke2fs(&self, src_tar: &Path, dst_image: &Path) -> Result<(), Ext4Error> {
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
        let num_inodes = inode_count_for(count_tar_entries(src_tar).await, fs_size_bytes);
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
            .arg(src_tar)
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

enum EmitSource {
    Host(PathBuf),
    Special {
        kind: SkippedSpecialKind,
        mtime: u64,
    },
}

/// Emit a deterministic rootfs tar from the host-content tree plus the
/// OCI metadata sidecar. Blocking — call from `spawn_blocking` in async
/// materialization code.
pub fn emit_tar(root: &Path, tree_meta: &TreeMetadata, dst_tar: &Path) -> Result<(), Ext4Error> {
    let file = File::create(dst_tar)?;
    let mut builder = tar::Builder::new(file);
    let entries = collect_emit_entries(root, tree_meta)?;
    let mut first_by_inode: HashMap<(u64, u64), String> = HashMap::new();

    for (rel, source) in entries {
        let tar_path = tar_path_for_rel(&rel);
        match source {
            EmitSource::Host(path) => {
                let fs_meta = std::fs::symlink_metadata(&path)?;
                let file_type = fs_meta.file_type();
                let (uid, gid, mode, xattrs) = tar_meta(&rel, Some(&fs_meta), tree_meta);
                let mtime = clamped_fs_mtime(&fs_meta);

                if file_type.is_dir() {
                    append_pax_xattrs(&mut builder, &xattrs)?;
                    let mut header =
                        tar_header(tar::EntryType::Directory, 0, mode, uid, gid, mtime);
                    builder.append_data(&mut header, &tar_path, std::io::empty())?;
                } else if file_type.is_symlink() {
                    append_pax_xattrs(&mut builder, &xattrs)?;
                    let mut header = tar_header(tar::EntryType::Symlink, 0, mode, uid, gid, mtime);
                    let target = std::fs::read_link(&path)?;
                    builder.append_link(&mut header, &tar_path, &target)?;
                } else if file_type.is_file() {
                    if fs_meta.nlink() > 1 {
                        let key = (fs_meta.dev(), fs_meta.ino());
                        if let Some(first) = first_by_inode.get(&key) {
                            let mut header =
                                tar_header(tar::EntryType::Link, 0, mode, uid, gid, mtime);
                            builder.append_link(&mut header, &tar_path, first)?;
                            continue;
                        }
                        first_by_inode.insert(key, rel.clone());
                    }
                    append_pax_xattrs(&mut builder, &xattrs)?;
                    let mut header = tar_header(
                        tar::EntryType::Regular,
                        fs_meta.len(),
                        mode,
                        uid,
                        gid,
                        mtime,
                    );
                    let file = File::open(&path)?;
                    builder.append_data(&mut header, &tar_path, BufReader::new(file))?;
                } else if file_type.is_fifo() {
                    append_pax_xattrs(&mut builder, &xattrs)?;
                    let mut header = tar_header(tar::EntryType::Fifo, 0, mode, uid, gid, mtime);
                    builder.append_data(&mut header, &tar_path, std::io::empty())?;
                }
            }
            EmitSource::Special { kind, mtime } => {
                let (uid, gid, mode, xattrs) = tar_meta(&rel, None, tree_meta);
                append_pax_xattrs(&mut builder, &xattrs)?;
                let mtime = mtime.min(DETERMINISTIC_EPOCH_SECS);
                let entry_type = match kind {
                    SkippedSpecialKind::Fifo => tar::EntryType::Fifo,
                    SkippedSpecialKind::Char { .. } => tar::EntryType::Char,
                    SkippedSpecialKind::Block { .. } => tar::EntryType::Block,
                };
                let mut header = tar_header(entry_type, 0, mode, uid, gid, mtime);
                match kind {
                    SkippedSpecialKind::Fifo => {}
                    SkippedSpecialKind::Char { major, minor }
                    | SkippedSpecialKind::Block { major, minor } => {
                        header.set_device_major(major)?;
                        header.set_device_minor(minor)?;
                    }
                }
                builder.append_data(&mut header, &tar_path, std::io::empty())?;
            }
        }
    }

    builder.finish()?;
    Ok(())
}

fn collect_emit_entries(
    root: &Path,
    tree_meta: &TreeMetadata,
) -> Result<BTreeMap<String, EmitSource>, Ext4Error> {
    let mut out = BTreeMap::new();
    out.insert(String::new(), EmitSource::Host(root.to_path_buf()));

    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut children = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            children.push(entry?.path());
        }
        children.sort();

        for path in children {
            let fs_meta = std::fs::symlink_metadata(&path)?;
            let rel = rel_string(root, &path)?;
            if fs_meta.file_type().is_dir() {
                stack.push(path.clone());
            }
            out.insert(rel, EmitSource::Host(path));
        }
    }

    for special in &tree_meta.skipped_specials {
        out.insert(
            special.path.clone(),
            EmitSource::Special {
                kind: special.kind.clone(),
                mtime: special.mtime,
            },
        );
    }

    Ok(out)
}

fn rel_string(root: &Path, path: &Path) -> Result<String, Ext4Error> {
    let rel = path.strip_prefix(root).map_err(|e| {
        Ext4Error::Io(std::io::Error::other(format!(
            "path escape while emitting tar: {e}"
        )))
    })?;
    Ok(rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/"))
}

fn tar_path_for_rel(rel: &str) -> PathBuf {
    if rel.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(rel)
    }
}

fn tar_meta(
    rel: &str,
    fs_meta: Option<&std::fs::Metadata>,
    tree_meta: &TreeMetadata,
) -> (u64, u64, u32, BTreeMap<String, Vec<u8>>) {
    if let Some(meta) = tree_meta.get(rel) {
        return (meta.uid, meta.gid, meta.mode, meta.xattrs.clone());
    }
    let mode = fs_meta
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o644);
    (0, 0, mode, BTreeMap::new())
}

fn tar_header(
    entry_type: tar::EntryType,
    size: u64,
    mode: u32,
    uid: u64,
    gid: u64,
    mtime: u64,
) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(entry_type);
    header.set_size(size);
    header.set_mode(mode);
    header.set_uid(uid);
    header.set_gid(gid);
    header.set_mtime(mtime);
    header
}

fn append_pax_xattrs<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    xattrs: &BTreeMap<String, Vec<u8>>,
) -> std::io::Result<()> {
    if xattrs.is_empty() {
        return Ok(());
    }
    let pax: Vec<(String, &[u8])> = xattrs
        .iter()
        .map(|(name, value)| (format!("SCHILY.xattr.{name}"), value.as_slice()))
        .collect();
    builder.append_pax_extensions(pax.iter().map(|(key, value)| (key.as_str(), *value)))
}

fn clamped_fs_mtime(meta: &std::fs::Metadata) -> u64 {
    if meta.mtime() < 0 {
        0
    } else {
        (meta.mtime() as u64).min(DETERMINISTIC_EPOCH_SECS)
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

/// Count tar payload entries (regular files, dirs, symlinks, special
/// files — one inode each). PAX/GNU metadata records are not rootfs
/// entries and do not need inodes.
async fn count_tar_entries(tar_path: &Path) -> u64 {
    let tar_path = tar_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> u64 {
        let Ok(file) = File::open(tar_path) else {
            return 0;
        };
        let mut archive = tar::Archive::new(BufReader::new(file));
        let Ok(entries) = archive.entries() else {
            return 0;
        };

        let mut n = 0u64;
        for entry in entries.flatten() {
            match entry.header().entry_type() {
                tar::EntryType::XHeader
                | tar::EntryType::XGlobalHeader
                | tar::EntryType::GNULongName
                | tar::EntryType::GNULongLink => {}
                _ => n = n.saturating_add(1),
            }
        }
        n
    })
    .await
    .unwrap_or(0)
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
/// Not following symlinks matches tar emission and `mke2fs -d <tar>`, which
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

    #[test]
    fn emit_tar_records_sidecar_metadata_xattrs_and_hardlinks() {
        use crate::flatten::{EntryMeta, TreeMetadata};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::fs::write(root.join("usr/bin/mount"), b"mount").unwrap();
        std::fs::hard_link(root.join("usr/bin/mount"), root.join("usr/bin/mount.link")).unwrap();
        std::os::unix::fs::symlink("usr/bin", root.join("bin")).unwrap();

        let old = filetime::FileTime::from_unix_time(1_000_000, 0); // 1970s
        filetime::set_file_mtime(root.join("usr/bin/mount"), old).unwrap();
        filetime::set_file_mtime(root.join("usr/bin/mount.link"), old).unwrap();

        let mut meta = TreeMetadata::default();
        let mut file_meta = EntryMeta::new(0, 0, 0o4755);
        file_meta
            .xattrs
            .insert("security.capability".into(), b"cap".to_vec());
        meta.insert("usr/bin/mount".into(), file_meta.clone());
        meta.insert("usr/bin/mount.link".into(), file_meta);
        meta.insert("bin".into(), EntryMeta::new(0, 0, 0o777));

        let out = tempfile::tempdir().unwrap();
        let tar_a = out.path().join("rootfs-a.tar");
        let tar_b = out.path().join("rootfs-b.tar");
        emit_tar(root, &meta, &tar_a).unwrap();
        emit_tar(root, &meta, &tar_b).unwrap();
        assert_eq!(
            std::fs::read(&tar_a).unwrap(),
            std::fs::read(&tar_b).unwrap(),
            "two emits of the same tree must be byte-identical"
        );

        let mut archive = tar::Archive::new(std::fs::File::open(tar_a).unwrap());
        let mut seen = BTreeMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let h = entry.header().clone();
            let mut xattrs = BTreeMap::new();
            if let Some(exts) = entry.pax_extensions().unwrap() {
                for ext in exts {
                    let ext = ext.unwrap();
                    let key = ext.key().unwrap();
                    if let Some(name) = key.strip_prefix("SCHILY.xattr.") {
                        xattrs.insert(name.to_string(), ext.value_bytes().to_vec());
                    }
                }
            }
            let link = h
                .link_name()
                .unwrap()
                .map(|p| p.to_string_lossy().into_owned());
            seen.insert(
                path,
                (
                    h.entry_type(),
                    h.uid().unwrap(),
                    h.gid().unwrap(),
                    h.mode().unwrap() & 0o7777,
                    h.mtime().unwrap(),
                    link,
                    xattrs,
                ),
            );
        }

        let root_entry = seen.get(".").expect("root entry");
        assert_eq!(root_entry.0, tar::EntryType::Directory);
        assert_eq!((root_entry.1, root_entry.2), (0, 0));

        let mount = seen.get("usr/bin/mount").expect("mount entry");
        assert_eq!(mount.0, tar::EntryType::Regular);
        assert_eq!((mount.1, mount.2, mount.3), (0, 0, 0o4755));
        assert_eq!(mount.4, 1_000_000, "old mtimes survive the clamp");
        assert_eq!(
            mount.6.get("security.capability").map(Vec::as_slice),
            Some(&b"cap"[..])
        );

        let link = seen.get("usr/bin/mount.link").expect("hardlink entry");
        assert_eq!(link.0, tar::EntryType::Link);
        assert_eq!(link.5.as_deref(), Some("usr/bin/mount"));

        let symlink = seen.get("bin").expect("symlink entry");
        assert_eq!(symlink.0, tar::EntryType::Symlink);
        assert_eq!(symlink.5.as_deref(), Some("usr/bin"));
    }

    #[test]
    fn emit_tar_clamps_new_mtimes_without_mutating_tree() {
        use crate::flatten::TreeMetadata;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("fresh"), b"now").unwrap();
        let before = std::fs::symlink_metadata(root.join("fresh"))
            .unwrap()
            .mtime();
        assert!(
            before >= DETERMINISTIC_EPOCH_SECS as i64,
            "fixture needs a post-epoch mtime"
        );

        let out = tempfile::tempdir().unwrap();
        let tar_path = out.path().join("rootfs.tar");
        emit_tar(root, &TreeMetadata::default(), &tar_path).unwrap();
        let after = std::fs::symlink_metadata(root.join("fresh"))
            .unwrap()
            .mtime();
        assert_eq!(after, before, "emit must not mutate host-tree mtimes");

        let mut archive = tar::Archive::new(std::fs::File::open(tar_path).unwrap());
        let fresh = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap())
            .find(|e| e.path().unwrap().to_string_lossy() == "fresh")
            .expect("fresh tar entry");
        assert_eq!(
            fresh.header().mtime().unwrap(),
            DETERMINISTIC_EPOCH_SECS,
            "new mtimes are clamped in tar headers"
        );
    }
}
