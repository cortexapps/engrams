//! ADR 0045 C2: the source-side dirty map — chunk-granular classification
//! of a paused FC guest's memory by walking `/proc/<fc_pid>/pagemap`.
//!
//! Under the substrate (guest RAM = `MAP_PRIVATE` of the per-template base
//! shm, UFFD `MISSING|MINOR`), page state encodes divergence directly:
//! divergent pages are ANONYMOUS (`UFFDIO_COPY`-installed or guest-COW),
//! clean pages are FILE-backed (`UFFDIO_CONTINUE` against the base), and
//! never-faulted pages are ABSENT. So the post-copy dirty map needs no
//! capture and no KVM-bitmap fork surface — the spike that proved the
//! kernel behaviors is `pagemap_probe.c` (a `substrate_kernel_capabilities`
//! CI gate).
//!
//! Classification rule (per 4 KiB page):
//!
//! ```text
//!   dirty = swapped(bit 62) || (present(bit 63) && !file_backed(bit 61))
//! ```
//!
//! - The swap arm goes BEYOND the spike: a COPY-installed/COW page the
//!   kernel swapped out reads `present=0`, and classifying it by
//!   present/file alone would silently drop dirty state. (Clean
//!   file-backed pages are never "swapped" — shmem page-cache eviction
//!   clears the PTE → absent.) The readv serving path faults swapped
//!   pages back in transparently.
//! - ABSENT = clean: under MAP_PRIVATE-of-shm + MISSING|MINOR an absent
//!   page was never faulted into this process, so its content is exactly
//!   what the session manifest resolves to (base / durable chunk / zero)
//!   — which the destination's normal `resolve()` reproduces identically.
//! - A chunk is dirty if ANY page in it is dirty. Over-approximation
//!   (anon-but-still-durable content) is absorbed by `ALT_SOURCE`: the
//!   server hashes the live bytes on request and demotes matches to the
//!   class-2 fetch.
//!
//! Everything here is blackout-critical (the scan sits between the pause
//! and the SEAL push) — reads are batched (4096 entries = 32 KiB per
//! `pread`) and the caller records `scan_ms` (R6: measure, never quote).

use std::io::{self, BufRead, BufReader};
use std::path::Path;

#[cfg(target_os = "linux")]
use engram_migrate_proto::SealBitmap;

/// Guest page size. Firecracker guest memory is 4 KiB-paged on x86_64
/// (the only architecture the fleet runs); `/proc/<pid>/pagemap` entries
/// are indexed by host-VA / this.
pub const PAGE_SIZE: u64 = 4096;

/// One base-shm-backed VMA of the FC process, in both address spaces:
/// `[start, end)` is the FC-process virtual range, `file_offset` is where
/// it sits in the base file — which IS the snapshot linear memory offset
/// (FC maps the base file at region offsets; mirrors
/// `GuestRegionUffdMapping`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestVma {
    pub start: u64,
    pub end: u64,
    pub file_offset: u64,
}

impl GuestVma {
    /// Length in bytes.
    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    /// True iff the VMA covers zero bytes (degenerate; never produced by
    /// the maps parser, but keeps clippy's `len-without-is-empty` honest).
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// Parse one `/proc/<pid>/maps` line, returning the VMA iff its pathname
/// equals `base_shm_path` exactly.
///
/// Line shape: `start-end perms offset dev inode          pathname`.
/// The first five columns never contain spaces; the pathname is the
/// remainder (it may contain spaces, so it is NOT `split_whitespace`'d).
/// A ` (deleted)` suffix means the base file was unlinked under a live
/// VM — that is a bug elsewhere (base_shm_gc holds an open-fd keep-set),
/// and such lines deliberately do NOT match.
pub fn parse_maps_line(line: &str, base_shm_path: &str) -> Option<GuestVma> {
    let mut rest = line;
    let mut cols = Vec::with_capacity(5);
    for _ in 0..5 {
        let trimmed = rest.trim_start_matches(' ');
        let end = trimmed.find(' ')?;
        cols.push(&trimmed[..end]);
        rest = &trimmed[end..];
    }
    let pathname = rest.trim_start_matches(' ').trim_end();
    if pathname != base_shm_path {
        return None;
    }

    let (start_s, end_s) = cols[0].split_once('-')?;
    let start = u64::from_str_radix(start_s, 16).ok()?;
    let end = u64::from_str_radix(end_s, 16).ok()?;
    let file_offset = u64::from_str_radix(cols[2], 16).ok()?;
    (end > start).then_some(GuestVma {
        start,
        end,
        file_offset,
    })
}

/// Read `/proc/<pid>/maps` and return the VMAs backed by `base_shm_path`,
/// sorted by `file_offset`. Empty result means the process doesn't map
/// the base (wrong pid, or a non-substrate restore) — callers treat that
/// as "cannot post-copy", not as an empty dirty set.
pub fn guest_vmas(pid: u32, base_shm_path: &Path) -> io::Result<Vec<GuestVma>> {
    let base = base_shm_path.to_string_lossy();
    let f = std::fs::File::open(format!("/proc/{pid}/maps"))?;
    let mut vmas = Vec::new();
    for line in BufReader::new(f).lines() {
        if let Some(vma) = parse_maps_line(&line?, &base) {
            vmas.push(vma);
        }
    }
    vmas.sort_by_key(|v| v.file_offset);
    Ok(vmas)
}

/// Find the substrate base file `pid` maps under `base_dir` — the
/// page server's discovery step (the host-agent knows the DIR from
/// config; the exact per-image file is whatever the restore derived).
/// Exactly one distinct base file is expected per FC process.
pub fn find_base_mapping(pid: u32, base_dir: &Path) -> io::Result<Option<std::path::PathBuf>> {
    let prefix = format!("{}/", base_dir.to_string_lossy().trim_end_matches('/'));
    let f = std::fs::File::open(format!("/proc/{pid}/maps"))?;
    for line in BufReader::new(f).lines() {
        let line = line?;
        if let Some(idx) = line.find(&prefix) {
            let path = line[idx..].trim_end();
            if !path.ends_with(" (deleted)") {
                return Ok(Some(std::path::PathBuf::from(path)));
            }
        }
    }
    Ok(None)
}

/// The per-page classification rule. `entry` is one raw 64-bit
/// `/proc/<pid>/pagemap` entry.
pub fn pagemap_entry_is_dirty(entry: u64) -> bool {
    let present = entry & (1 << 63) != 0;
    let swapped = entry & (1 << 62) != 0;
    let file_backed = entry & (1 << 61) != 0;
    swapped || (present && !file_backed)
}

/// Pagemap entries batched per `pread` (4096 entries = 32 KiB covering
/// 16 MiB of guest memory per syscall).
#[cfg(target_os = "linux")]
const PAGEMAP_BATCH_ENTRIES: usize = 4096;

/// Walk `/proc/<pid>/pagemap` over `vmas` and build the chunk-granular
/// [`SealBitmap`]: bit `i` set ⇔ any page of chunk `i` is dirty. Offsets
/// not covered by any VMA (e.g. the sub-1MiB BIOS-hole split between
/// regions) contribute clean. The caller must hold the VM paused — a
/// running guest would race the scan.
#[cfg(target_os = "linux")]
pub fn scan_dirty_chunks(
    pid: u32,
    vmas: &[GuestVma],
    chunk_size: u64,
    total_bytes: u64,
) -> io::Result<SealBitmap> {
    use std::os::unix::fs::FileExt;

    let chunk_count = total_bytes.div_ceil(chunk_size);
    let mut seal = SealBitmap::new(chunk_size, chunk_count);
    let pagemap = std::fs::File::open(format!("/proc/{pid}/pagemap"))?;
    let mut buf = vec![0u8; PAGEMAP_BATCH_ENTRIES * 8];

    for vma in vmas {
        let mut va = vma.start;
        while va < vma.end {
            let pages_left = ((vma.end - va) / PAGE_SIZE) as usize;
            let n = pages_left.min(PAGEMAP_BATCH_ENTRIES);
            let byte_len = n * 8;
            pagemap.read_exact_at(&mut buf[..byte_len], (va / PAGE_SIZE) * 8)?;

            let mut i = 0usize;
            while i < n {
                let entry = u64::from_le_bytes(buf[i * 8..i * 8 + 8].try_into().unwrap());
                if pagemap_entry_is_dirty(entry) {
                    let offset = vma.file_offset + (va - vma.start) + (i as u64) * PAGE_SIZE;
                    if offset < total_bytes {
                        let chunk = offset / chunk_size;
                        seal.set(chunk);
                        // Any-page rule satisfied: skip the rest of this
                        // chunk (within the batch; the next batch
                        // re-checks cheaply via the bitmap).
                        let chunk_end_off = (chunk + 1) * chunk_size;
                        let pages_to_skip =
                            (chunk_end_off.saturating_sub(offset)).div_ceil(PAGE_SIZE) as usize;
                        i += pages_to_skip.max(1);
                        continue;
                    }
                }
                i += 1;
            }
            va += (n as u64) * PAGE_SIZE;
        }
    }
    Ok(seal)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "/var/lib/engram/shm/abc-v3.base";

    #[test]
    fn maps_line_matching_base_path_parses() {
        let line = format!("7f1200000000-7f1240000000 rw-p 00200000 00:01 12345  {BASE}");
        let vma = parse_maps_line(&line, BASE).expect("should match");
        assert_eq!(vma.start, 0x7f1200000000);
        assert_eq!(vma.end, 0x7f1240000000);
        assert_eq!(vma.file_offset, 0x200000);
        assert_eq!(vma.len(), 0x40000000);
    }

    #[test]
    fn maps_line_other_paths_and_anon_do_not_match() {
        for line in [
            format!("7f1200000000-7f1240000000 rw-p 00000000 00:01 12345  {BASE}.other"),
            "7f1200000000-7f1240000000 rw-p 00000000 00:00 0 ".to_string(),
            "7f1200000000-7f1240000000 rw-p 00000000 00:01 12345  [heap]".to_string(),
            // Unlinked base = a bug elsewhere; deliberately no match.
            format!("7f1200000000-7f1240000000 rw-p 00000000 00:01 12345  {BASE} (deleted)"),
        ] {
            assert!(
                parse_maps_line(&line, BASE).is_none(),
                "should not match: {line}"
            );
        }
    }

    #[test]
    fn maps_path_with_spaces_matches_exactly() {
        let spaced = "/var/lib/engram/shm/with space-v1.base";
        let line = format!("1000-2000 rw-p 00000000 00:01 1  {spaced}");
        assert!(parse_maps_line(&line, spaced).is_some());
        assert!(parse_maps_line(&line, BASE).is_none());
    }

    #[test]
    fn classification_rule_table() {
        const PRESENT: u64 = 1 << 63;
        const SWAPPED: u64 = 1 << 62;
        const FILE: u64 = 1 << 61;
        // (entry, dirty?)
        for (entry, want, label) in [
            (PRESENT, true, "present anon = COPY/COW divergence"),
            (PRESENT | FILE, false, "present file = clean CONTINUE"),
            (0u64, false, "absent = never faulted"),
            (
                SWAPPED,
                true,
                "swapped anon = divergence the kernel paged out",
            ),
            (FILE, false, "absent file-flagged = clean"),
        ] {
            assert_eq!(pagemap_entry_is_dirty(entry), want, "{label}");
        }
    }
}
