//! Deterministic ext4 SIZING — the inputs the streaming packer feeds
//! mkext4 (ADR 0093 retired the mke2fs shell-out; ADR 0036's
//! determinism constants and the size/inode quantizers live on here).
//! Tree → image packing for fixtures is `stream_pack::pack_tree`.

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
pub(crate) const DETERMINISTIC_FS_UUID: &str = "00000000-e9a4-4a11-8036-000000000036";
pub(crate) const DETERMINISTIC_HASH_SEED: &str = "00000000-5eed-4a11-8036-000000000036";
/// 2024-01-01T00:00:00Z. e2fsprogs (≥1.45) reads `SOURCE_DATE_EPOCH`
/// and (a) stamps superblock mkfs/write times from it instead of the
/// wall clock, and (b) clamps inode timestamps newer than it — which
/// covers files injected at materialize time (the init shim). The
/// streaming packer clamps at declare time (`stream_pack`); mkext4
/// stamps superblock times from this same epoch.
pub const DETERMINISTIC_EPOCH_SECS: u64 = 1_704_067_200;

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
}
