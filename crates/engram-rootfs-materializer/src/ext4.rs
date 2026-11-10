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

const GROUP_BYTES: u64 = 128 * 1024 * 1024;
const BLOCKS_PER_GROUP: u64 = 32_768;
/// The stable layout's window in 4 KiB blocks: one disk chunk.
const WINDOW_BLOCKS: u64 = engram_chunk_store::DEFAULT_DISK_CHUNK_SIZE / 4096;
/// The inode numbers in one stable-layout window (256 B per inode).
const INODES_PER_WINDOW: u64 = WINDOW_BLOCKS * 16;
/// The most inodes a block group can have under the stable layout. A
/// flex span's inode tables (16 groups of `inodes_per_group / 16` blocks,
/// so `inodes_per_group` blocks) sit in the span's first group behind one
/// window of superblock, descriptors and bitmaps.
const MAX_INODES_PER_GROUP: u64 = BLOCKS_PER_GROUP - WINDOW_BLOCKS;

/// The image size for `dir_size_bytes` of data in `entry_count` entries:
/// the larger of 4 × the data and the block groups that
/// [`recommended_inodes`] needs, rounded up to 256 MiB.
///
/// - The stable layout pads segments with zero windows and can need up
///   to twice the used bytes of logical size. The padding stays free, so
///   the guest also gets at least the data's size as working room.
/// - A group holds at most [`MAX_INODES_PER_GROUP`] inodes, so a tree of
///   many small or empty files is sized by its entries, not its bytes.
/// - The 256 MiB bands keep the block-group geometry the same while the
///   tree drifts by less than a band. A geometry change moves every
///   inode table and so rewrites every chunk. Whole bands are also whole
///   512-byte sectors, which VZ requires of a disk image.
///
/// The size beyond the data is zero windows, which manifests elide and
/// sparse files do not store.
pub fn recommended_size(dir_size_bytes: u64, entry_count: u64) -> u64 {
    const BAND: u64 = 256 * 1024 * 1024;
    let for_inodes = recommended_inodes(entry_count)
        .div_ceil(MAX_INODES_PER_GROUP)
        .saturating_mul(GROUP_BYTES);
    let raw = dir_size_bytes.saturating_mul(4).max(for_inodes);
    raw.saturating_add(BAND - 1) & !(BAND - 1)
}

/// The inode count for a tree of `entry_count` entries, in 128 Ki bands.
///
/// The stable layout numbers inodes in window-aligned bands from the
/// second window (inode 65537) up, and pads each band to a power of two
/// of windows, so a band of `u` inodes spans at most `2·u` plus two
/// windows. Band boundaries come from path hashes and average one per
/// 524288 inodes. Allowing four times that many bands, plus two, the
/// highest number is at most `65536 + 2·E + 2 windows · (E/131072 + 2)`,
/// which is `3·E + 5 windows`. A count that covers the highest number
/// means mkext4 never grows the inode tables: growing moves every table,
/// rewrites every chunk, and can overflow the groups that
/// [`recommended_size`] sized. The unused numbers stay free for the
/// guest. The 128 Ki bands keep the count the same while the entry count
/// drifts.
pub fn recommended_inodes(entry_count: u64) -> u64 {
    const BAND: u64 = 128 * 1024;
    let raw = entry_count
        .saturating_mul(3)
        .saturating_add(5 * INODES_PER_WINDOW);
    raw.saturating_add(BAND - 1) & !(BAND - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommended_size_is_four_times_the_data_in_256_mib_bands() {
        const MIB: u64 = 1024 * 1024;
        assert_eq!(recommended_size(512 * MIB, 0), 2048 * MIB);
        assert_eq!(recommended_size(513 * MIB, 0), 2048 * MIB + 256 * MIB);
    }

    /// A tree of empty files has no bytes; its entries size the image,
    /// so every inode the stable layout can number fits its groups.
    #[test]
    fn recommended_size_holds_the_inodes_of_a_tree_of_empty_files() {
        for entries in [0u64, 1, 170_000, 1_000_000] {
            let groups = recommended_size(0, entries) / GROUP_BYTES;
            assert!(
                groups * MAX_INODES_PER_GROUP >= recommended_inodes(entries),
                "{entries} entries need {} inodes, {groups} groups hold fewer",
                recommended_inodes(entries)
            );
        }
    }

    #[test]
    fn recommended_inodes_covers_stable_numbering_and_quantizes() {
        const BAND: u64 = 128 * 1024;
        for count in [0u64, 1, 50_000, 300_000, 1_000_000, 5_000_000] {
            let n = recommended_inodes(count);
            assert_eq!(n % BAND, 0, "must be band-aligned for count {count}");
            assert!(
                n >= count * 3 + 5 * INODES_PER_WINDOW,
                "{n} inodes must cover the stable numbering of {count} entries"
            );
        }
    }

    #[test]
    fn recommended_size_does_not_overflow() {
        // Adversarial input doesn't panic.
        assert!(recommended_size(u64::MAX, u64::MAX) > 0);
    }

    /// ADR 0036: sizes snap to 256 MiB bands, so small source-tree
    /// drift maps to the same filesystem geometry and cross-bake chunk
    /// dedup survives.
    #[test]
    fn recommended_size_quantizes_large_images_to_256mib_bands() {
        const MIB: u64 = 1024 * 1024;
        let a = recommended_size(4900 * MIB, 0);
        let b = recommended_size(4900 * MIB + 8 * 1024, 0); // +8 KB tree drift
        assert_eq!(a, b, "small drift must not change the fs size");
        assert_eq!(a % (256 * MIB), 0, "large sizes snap to 256 MiB");
        // Still never smaller than the raw requirement.
        assert!(a >= 4 * 4900 * MIB);
        // And a genuinely larger tree eventually crosses a band edge.
        assert!(recommended_size(5200 * MIB, 0) > a);
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
                recommended_size(s, 0) % 4096,
                0,
                "recommended_size({s}, 0) must be 4 KiB aligned for VZ"
            );
        }
    }
}
