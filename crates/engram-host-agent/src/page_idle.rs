//! Best-effort recency hints from Linux idle-page tracking. These hints never
//! decide which bytes must be transferred. Invariant: a hot-set hint must never
//! fail, delay, or block a move, resume, drain, or blackout. See the kernel's
//! Documentation/admin-guide/mm/idle_page_tracking.rst.

use std::collections::{BTreeMap, HashSet};
use std::io;

pub const BITMAP_PATH: &str = "/sys/kernel/mm/page_idle/bitmap";
pub const MAX_HOT_CHUNKS: usize = 4096;

/// A native-endian bitmap word at an 8-byte aligned file offset.
pub fn pfn_word_bit(pfn: u64) -> (u64, u64) {
    ((pfn / 64) * 8, 1 << (pfn % 64))
}

/// Return only visible PFNs of private anonymous pages. Shared/file pages
/// cannot provide per-guest hotness and must not be marked or counted.
pub fn present_pfn(entry: u64) -> Option<u64> {
    let pfn = entry & ((1 << 55) - 1);
    (entry & (1 << 63) != 0 && entry & (1 << 61) == 0 && entry & (1 << 56) != 0 && pfn != 0)
        .then_some(pfn)
}

/// Count accessed pages per chunk. The injected reader returns a native-endian
/// bitmap word. Equal counts keep offset order.
#[derive(Default)]
pub struct HotPages {
    counts: BTreeMap<u64, u64>,
}

impl HotPages {
    pub fn observe(
        &mut self,
        offset: u64,
        entry: u64,
        chunk_size: u64,
        mut read: impl FnMut(u64) -> io::Result<u64>,
    ) -> io::Result<()> {
        if let Some(pfn) = present_pfn(entry) {
            let (word, bit) = pfn_word_bit(pfn);
            if read(word)? & bit == 0 {
                *self
                    .counts
                    .entry(offset / chunk_size * chunk_size)
                    .or_default() += 1;
            }
        }
        Ok(())
    }

    pub fn ordered(&self) -> Vec<u64> {
        let mut counts: Vec<_> = self.counts.iter().map(|(&o, &n)| (o, n)).collect();
        counts.sort_unstable_by_key(|&(offset, count)| (std::cmp::Reverse(count), offset));
        counts.into_iter().map(|(offset, _)| offset).collect()
    }
}

/// Resolve offsets without repeated manifest scans. Deduplicate equal hashes
/// before the idle-only cap. Preserve the complete bounded handler-trace tier.
pub fn hot_hashes(
    manifest: &engram_chunk_store::Manifest,
    offsets: &[u64],
    fallback: &[[u8; 32]],
) -> Vec<[u8; 32]> {
    let by_offset: BTreeMap<_, _> = manifest
        .chunks
        .iter()
        .map(|c| (c.offset, *c.hash.as_bytes()))
        .collect();
    let mut seen = HashSet::new();
    let mut hot: Vec<_> = fallback
        .iter()
        .copied()
        .filter(|h| seen.insert(*h))
        .collect();
    hot.extend(
        offsets
            .iter()
            .filter_map(|o| by_offset.get(o))
            .copied()
            .filter(|h| seen.insert(*h))
            .take(MAX_HOT_CHUNKS),
    );
    hot
}

#[cfg(target_os = "linux")]
pub fn open_bitmap() -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(BITMAP_PATH)
}

/// Log optional-feature failures only once per process, including read failures.
#[cfg(target_os = "linux")]
pub fn note_unavailable(error: &io::Error) {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::info!(%error, "page_idle unavailable; using handler working-set trace");
    }
}

/// Set only bits for present guest pages. Writes are OR operations in the
/// kernel: no read-modify-write, and no changes to unrelated idle bits.
#[cfg(target_os = "linux")]
pub fn mark(pid: u32, vmas: &[crate::dirty_map::GuestVma]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    let bitmap = open_bitmap()?;
    let mut words = BTreeMap::<u64, u64>::new();
    let mut heads = CompoundHeads::new()?;
    crate::dirty_map::walk_pagemap(pid, vmas, |_, entry| {
        if let Some(pfn) = present_pfn(entry) {
            let (word, bit) = pfn_word_bit(heads.head(pfn)?);
            *words.entry(word).or_default() |= bit;
        }
        Ok(())
    })?;
    for (word, bits) in words {
        bitmap.write_all_at(&bits.to_ne_bytes(), word)?;
    }
    Ok(())
}

/// Read the captured PFNs after export registration. The source stays paused,
/// but host reclaim can still change PFNs. These are ordering hints only.
#[cfg(target_os = "linux")]
pub fn scan(pages: &[(u64, u64)], chunk_size: u64) -> io::Result<Vec<u64>> {
    use std::os::unix::fs::FileExt;
    let bitmap = open_bitmap()?;
    let mut words = std::collections::HashMap::new();
    let mut hot = HotPages::default();
    let mut heads = CompoundHeads::new()?;
    for &(offset, pfn) in pages {
        let head = heads.head(pfn)?;
        hot.observe(offset, (1 << 63) | (1 << 56) | head, chunk_size, |word| {
            if let Some(bits) = words.get(&word) {
                return Ok(*bits);
            }
            let mut bytes = [0; 8];
            bitmap.read_exact_at(&mut bytes, word)?;
            let bits = u64::from_ne_bytes(bytes);
            words.insert(word, bits);
            Ok(bits)
        })?;
    }
    Ok(hot.ordered())
}

/// Idle bits exist only on compound heads. Cache kpageflags blocks so walking
/// contiguous huge pages does not issue one syscall per base page.
#[cfg(target_os = "linux")]
struct CompoundHeads {
    file: std::fs::File,
    blocks: std::collections::HashMap<u64, Vec<u8>>,
    heads: BTreeMap<u64, u32>,
}

#[cfg(target_os = "linux")]
impl CompoundHeads {
    fn new() -> io::Result<Self> {
        Ok(Self {
            file: std::fs::File::open("/proc/kpageflags")?,
            blocks: Default::default(),
            heads: Default::default(),
        })
    }

    fn flags(&mut self, pfn: u64) -> io::Result<u64> {
        use std::os::unix::fs::FileExt;
        let block = pfn / 512;
        let bytes = match self.blocks.entry(block) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let mut bytes = vec![0; 4096];
                self.file.read_exact_at(&mut bytes, block * 4096)?;
                e.insert(bytes)
            }
        };
        let pos = (pfn % 512) as usize * 8;
        Ok(u64::from_ne_bytes(bytes[pos..pos + 8].try_into().unwrap()))
    }

    fn head(&mut self, pfn: u64) -> io::Result<u64> {
        // Split borrows: the extent cache needs only a kpageflags reader.
        let mut extents = std::mem::take(&mut self.heads);
        let result = compound_head(&mut extents, pfn, |p| self.flags(p));
        self.heads = extents;
        result
    }
}

/// Cache complete compound extents as (head, order). Each tail is examined once,
/// then ascending tails resolve with a range lookup, without per-tail inserts.
#[cfg(any(target_os = "linux", test))]
fn compound_head(
    extents: &mut BTreeMap<u64, u32>,
    pfn: u64,
    mut flags: impl FnMut(u64) -> io::Result<u64>,
) -> io::Result<u64> {
    const HEAD: u64 = 1 << 15;
    const TAIL: u64 = 1 << 16;
    if let Some((&head, &order)) = extents.range(..=pfn).next_back() {
        if pfn - head < 1u64 << order {
            return Ok(head);
        }
    }
    let f = flags(pfn)?;
    if f & (HEAD | TAIL) == 0 {
        return Ok(pfn);
    }
    let mut head = pfn;
    while flags(head)? & TAIL != 0 {
        head = head
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("compound tail without head"))?;
    }
    if flags(head)? & HEAD == 0 {
        return Err(io::Error::other("invalid compound head"));
    }
    let mut end = head + 1;
    while flags(end)? & TAIL != 0 {
        end += 1;
    }
    let pages = end - head;
    if !pages.is_power_of_two() {
        return Err(io::Error::other("invalid compound extent"));
    }
    extents.insert(head, pages.trailing_zeros());
    Ok(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compound_extent_lookup_is_linear_and_validates_head() {
        let mut extents = BTreeMap::new();
        let mut reads = 0;
        for pfn in 513..1024 {
            assert_eq!(
                compound_head(&mut extents, pfn, |p| {
                    reads += 1;
                    Ok(if p == 512 {
                        1 << 15
                    } else if p < 1024 {
                        1 << 16
                    } else {
                        0
                    })
                })
                .unwrap(),
                512
            );
        }
        assert!(reads < 520, "{reads} kpageflags operations");
        assert_eq!(extents.len(), 1);
        assert!(compound_head(&mut BTreeMap::new(), 2, |p| Ok(if p == 2 {
            1 << 16
        } else {
            0
        }))
        .is_err());
    }

    #[test]
    fn shared_pagemap_entries_are_neither_marked_nor_counted() {
        let mut hot = HotPages::default();
        for entry in [(1 << 63) | 42, (1 << 63) | (1 << 56) | (1 << 61) | 42] {
            assert_eq!(present_pfn(entry), None); // mark and PFN capture use this filter
            hot.observe(0, entry, 4096, |_| panic!("shared page read"))
                .unwrap();
        }
        assert!(hot.ordered().is_empty());
    }

    #[test]
    fn pfn_alignment_presence_and_native_bits() {
        assert_eq!(pfn_word_bit(63), (0, 1 << 63));
        assert_eq!(pfn_word_bit(64), (8, 1));
        assert_eq!(pfn_word_bit(129), (16, 2));
        assert_eq!(present_pfn(1 << 63), None);
        assert_eq!(present_pfn((1 << 62) | 42), None);
        assert_eq!(present_pfn((1 << 63) | (1 << 56) | (1 << 61) | 42), None);
    }

    #[test]
    fn fake_reader_counts_accessed_pages_and_breaks_ties_by_offset() {
        let mut pages = HotPages::default();
        for (offset, pfn) in [(0, 64), (4096, 65), (8192, 66), (12288, 67), (16384, 68)] {
            pages
                .observe(offset, (1 << 63) | (1 << 56) | pfn, 8192, |word| {
                    assert_eq!(word, 8);
                    Ok(1 << 2) // page 66 alone stayed idle
                })
                .unwrap();
        }
        pages
            .observe(24576, 0, 8192, |_| panic!("absent page read"))
            .unwrap();
        assert_eq!(pages.ordered(), vec![0, 8192, 16384]);
    }

    #[test]
    fn hashes_keep_trace_first_skip_zero_chunks_deduplicate_and_cap() {
        use engram_chunk_store::manifest::{ChunkHash, ChunkRef, ChunkSize, ManifestKind};
        let chunks: Vec<_> = (0..5000_u64)
            .map(|i| {
                let mut bytes = [0; 32];
                bytes[..8].copy_from_slice(&i.to_le_bytes());
                ChunkRef {
                    offset: i * 4096,
                    hash: ChunkHash::from_bytes(bytes),
                }
            })
            .collect();
        let manifest = engram_chunk_store::Manifest {
            schema_version: 1,
            kind: ManifestKind::Memory,
            chunk_size: ChunkSize::bytes(4096),
            total_bytes: 5001 * 4096,
            chunks,
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let first = *manifest.chunks[5].hash.as_bytes();
        let offsets: Vec<_> = (0..=5000_u64).rev().map(|i| i * 4096).collect();
        let hot = hot_hashes(&manifest, &offsets, &[first, first]);
        assert_eq!(hot.len(), MAX_HOT_CHUNKS + 1);
        let whole: Vec<_> = manifest.chunks.iter().map(|c| *c.hash.as_bytes()).collect();
        assert_eq!(hot_hashes(&manifest, &[], &whole), whole);
        assert_eq!(hot[0], first);
        assert_eq!(hot[1], *manifest.chunks[4999].hash.as_bytes());
        assert_eq!(hot.iter().filter(|h| **h == first).count(), 1);
        assert_eq!(hot_hashes(&manifest, &[], &[first]), vec![first]);
        assert!(hot_hashes(&manifest, &[5000 * 4096], &[]).is_empty());
    }

    #[test]
    fn idle_read_failure_is_reported() {
        let mut hot = HotPages::default();
        assert!(hot
            .observe(0, (1 << 63) | (1 << 56) | 1, 4096, |_| Err(
                io::ErrorKind::PermissionDenied.into()
            ))
            .is_err());
        assert!(hot.ordered().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn anonymous_mapping_accesses_clear_idle_bits() {
        use std::os::unix::fs::FileExt;
        // SAFETY: geteuid has no arguments or memory effects.
        if unsafe { libc::geteuid() } != 0 || !std::path::Path::new(BITMAP_PATH).exists() {
            eprintln!("SKIP: page_idle requires root and CONFIG_IDLE_PAGE_TRACKING");
            return;
        }
        let Ok(bitmap) = open_bitmap() else {
            eprintln!("SKIP: page_idle is not writable");
            return;
        };
        // SAFETY: anonymous mapping, owned until munmap below.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                16 * 4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        struct Mapping(*mut libc::c_void);
        impl Drop for Mapping {
            fn drop(&mut self) {
                // SAFETY: this guard owns the complete mapping.
                unsafe {
                    libc::munmap(self.0, 16 * 4096);
                }
            }
        }
        let _guard = Mapping(ptr);
        // SAFETY: mapping is writable and the guard outlives this slice.
        let mapping = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), 16 * 4096) };
        // Avoid THP: only the compound head carries an idle flag.
        // SAFETY: this is the complete live mapping and advice changes no ownership.
        assert_eq!(
            unsafe {
                libc::madvise(
                    mapping.as_mut_ptr().cast(),
                    mapping.len(),
                    libc::MADV_NOHUGEPAGE,
                )
            },
            0
        );
        for i in 0..16 {
            mapping[i * 4096] = 1;
        }
        let start = mapping.as_ptr() as u64;
        let vmas = [crate::dirty_map::GuestVma {
            start,
            end: start + mapping.len() as u64,
            file_offset: 0,
        }];
        let mut pfns = Vec::new();
        crate::dirty_map::walk_pagemap(std::process::id(), &vmas, |_, entry| {
            pfns.push(present_pfn(entry));
            Ok(())
        })
        .unwrap();
        if pfns.iter().any(Option::is_none) {
            eprintln!("SKIP: pagemap PFNs hidden (CAP_SYS_ADMIN required)");
            return;
        }
        // Newly faulted pages can still be in a per-CPU LRU batch. Establish
        // an idle baseline before testing access-bit changes.
        let mut marked = false;
        for _ in 0..100 {
            mark(std::process::id(), &vmas).unwrap();
            marked = pfns.iter().flatten().all(|pfn| {
                let (word, bit) = pfn_word_bit(*pfn);
                let mut bytes = [0; 8];
                bitmap.read_exact_at(&mut bytes, word).unwrap();
                u64::from_ne_bytes(bytes) & bit != 0
            });
            if marked {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(marked, "anonymous pages must accept idle marks");
        for i in 0..8 {
            // SAFETY: each address is within the live mapping; volatile prevents elision.
            unsafe {
                std::ptr::write_volatile(mapping.as_mut_ptr().add(i * 4096), 2);
            }
        }
        for (i, pfn) in pfns.iter().flatten().enumerate() {
            let (word, bit) = pfn_word_bit(*pfn);
            let mut bytes = [0; 8];
            bitmap.read_exact_at(&mut bytes, word).unwrap();
            assert_eq!(
                u64::from_ne_bytes(bytes) & bit == 0,
                i < 8,
                "access classification for page {i}"
            );
        }
    }
}
