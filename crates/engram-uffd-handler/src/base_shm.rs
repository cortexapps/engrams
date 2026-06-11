//! ADR 0045 unified memory substrate (v2b): the per-template base shm file.
//!
//! Guest memory is `MAP_PRIVATE` of this file (the forked FC maps it;
//! registration is `MISSING|MINOR`). This handler lazily populates it with
//! *canonical* chunk bytes — one copy per host, shared by every
//! same-template microVM through the page cache via `UFFDIO_CONTINUE` —
//! and NEVER writes session-divergent bytes here (those install privately
//! via `UFFDIO_COPY`). Guest writes COW in the kernel and never reach
//! this file either, so its content is canonical by construction.
//!
//! Concurrency: several handlers (one per sibling VM) populate the same
//! file. That's safe without coordination — every writer writes the same
//! canonical bytes at the same offsets (idempotent), and the
//! populated-probe (`SEEK_HOLE`) only skips work that some sibling has
//! already completed. A racing CONTINUE against a half-written chunk
//! cannot happen for the *faulting* sandbox because we always pwrite the
//! full range before CONTINUE-ing it; a sibling that observes the range
//! as populated observes it post-`pwrite` (shmem pwrite publishes whole
//! pages).

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum BaseShmError {
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    SetLen {
        path: PathBuf,
        len: u64,
        source: std::io::Error,
    },
    Write {
        offset: u64,
        source: std::io::Error,
    },
}

impl std::fmt::Display for BaseShmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open { path, source } => {
                write!(f, "open/create base shm {}: {source}", path.display())
            }
            Self::SetLen { path, len, source } => {
                write!(f, "size base shm {} to {len}: {source}", path.display())
            }
            Self::Write { offset, source } => write!(f, "pwrite base shm at {offset}: {source}"),
        }
    }
}

impl std::error::Error for BaseShmError {}

#[derive(Debug)]
pub struct BaseShm {
    file: File,
    path: PathBuf,
    total_bytes: u64,
    /// Sorted, non-overlapping `[start, end)` byte ranges holding data
    /// (vs holes). `None` until the first `is_populated` probe builds it
    /// with one whole-file `SEEK_DATA`/`SEEK_HOLE` walk; `write_chunk`
    /// keeps it current. See `is_populated` for why this exists.
    data_ranges: std::sync::Mutex<Option<Vec<(u64, u64)>>>,
}

impl BaseShm {
    /// Open-or-create the base file and size it to the canonical
    /// manifest's `total_bytes`. Must run before the UDS listener binds:
    /// the forked FC `open(O_RDONLY)` + `mmap`s this exact size at load,
    /// which the caller orders after the socket appears.
    pub fn open(path: &Path, total_bytes: u64) -> Result<Self, BaseShmError> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|source| BaseShmError::Open {
                path: path.to_path_buf(),
                source,
            })?;
        let cur = file
            .metadata()
            .map_err(|source| BaseShmError::Open {
                path: path.to_path_buf(),
                source,
            })?
            .len();
        // Grow-only: a sibling may have created it already at the same
        // size; never shrink a file FC processes may be mapping.
        if cur < total_bytes {
            file.set_len(total_bytes)
                .map_err(|source| BaseShmError::SetLen {
                    path: path.to_path_buf(),
                    len: total_bytes,
                    source,
                })?;
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
            total_bytes,
            data_ranges: std::sync::Mutex::new(None),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Is `[offset, offset+len)` fully populated (no holes)?
    ///
    /// Answered from an in-memory data-range map built ONCE (one
    /// `SEEK_DATA`/`SEEK_HOLE` walk of the whole file on first probe),
    /// NOT a per-probe `lseek(SEEK_HOLE)`. The per-probe lseek was the
    /// teleport tail-latency killer (ADR 0045 C1): on a freshly-rolled
    /// host the kernel's extent map is cold and a DENSE (pre-warmed)
    /// file makes each `SEEK_HOLE` walk every extent from `offset` to
    /// the next hole — thousands of first-touch faults compounded into
    /// a ~46 s post-restore guest crawl (prod canary 1f64052e), gone on
    /// the second restore once the extent cache warmed.
    ///
    /// Staleness is one-sided and safe: a SIBLING handler populating a
    /// chunk after our scan makes us refetch + rewrite the same
    /// canonical bytes (idempotent, see the module doc) — never serve
    /// a hole as populated, because only `write_chunk` (which updates
    /// the map) turns a hole into data and nothing ever punches holes.
    pub fn is_populated(&self, offset: u64, len: u64) -> bool {
        let end = offset.saturating_add(len).min(self.total_bytes);
        let mut ranges = self.data_ranges.lock().expect("data_ranges poisoned");
        let ranges = match ranges.as_mut() {
            Some(r) => r,
            None => {
                let scanned = self.scan_data_ranges().unwrap_or_default();
                ranges.insert(scanned)
            }
        };
        // Sorted, non-overlapping [start, end) ranges: the candidate is
        // the last range starting at-or-before `offset`.
        let idx = ranges.partition_point(|r| r.0 <= offset);
        idx > 0 && ranges[idx - 1].1 >= end
    }

    /// One full-file `SEEK_DATA`/`SEEK_HOLE` walk — O(extents) total,
    /// paid once, instead of O(extents) per fault.
    fn scan_data_ranges(&self) -> std::io::Result<Vec<(u64, u64)>> {
        let fd = self.file.as_raw_fd();
        let mut ranges = Vec::new();
        let mut pos: u64 = 0;
        while pos < self.total_bytes {
            // SAFETY: plain lseek on an fd we own; no memory is touched.
            let data = unsafe { libc::lseek64(fd, pos as libc::off64_t, libc::SEEK_DATA) };
            if data < 0 {
                // ENXIO: no data at-or-after pos — the tail is one hole.
                break;
            }
            let data = data as u64;
            let hole = unsafe { libc::lseek64(fd, data as libc::off64_t, libc::SEEK_HOLE) };
            if hole < 0 {
                return Err(std::io::Error::last_os_error());
            }
            ranges.push((data, (hole as u64).min(self.total_bytes)));
            pos = hole as u64;
        }
        Ok(ranges)
    }

    /// Write canonical chunk bytes at `offset`. Idempotent across
    /// concurrent sibling handlers (same bytes, same offset). Records
    /// the range in the populated map so the next `is_populated` probe
    /// sees it without re-scanning.
    pub fn write_chunk(&self, offset: u64, bytes: &[u8]) -> Result<(), BaseShmError> {
        self.file
            .write_all_at(bytes, offset)
            .map_err(|source| BaseShmError::Write { offset, source })?;
        let end = offset + bytes.len() as u64;
        let mut ranges = self.data_ranges.lock().expect("data_ranges poisoned");
        if let Some(ranges) = ranges.as_mut() {
            // Extend the preceding range when adjacent/overlapping, else
            // insert in order — then merge forward so abutting ranges
            // fuse (a probe requires ONE covering range).
            let idx = ranges.partition_point(|r| r.0 <= offset);
            let lo = if idx > 0 && ranges[idx - 1].1 >= offset {
                ranges[idx - 1].1 = ranges[idx - 1].1.max(end);
                idx - 1
            } else {
                ranges.insert(idx, (offset, end));
                idx
            };
            while lo + 1 < ranges.len() && ranges[lo + 1].0 <= ranges[lo].1 {
                ranges[lo].1 = ranges[lo].1.max(ranges[lo + 1].1);
                ranges.remove(lo + 1);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The populated map must agree with the file's real hole structure
    /// (pre-existing data from a "pre-warm"), and `write_chunk` must
    /// update it without a re-scan.
    #[test]
    fn populated_map_tracks_holes_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.base");
        const CHUNK: u64 = 64 * 1024; // fs hole granularity friendly
        let total = 8 * CHUNK;

        // Pre-warm chunks 0 and 2 out-of-band (a sibling / image_prefetch).
        {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)
                .unwrap();
            f.set_len(total).unwrap();
            f.write_all_at(&vec![0xAAu8; CHUNK as usize], 0).unwrap();
            f.write_all_at(&vec![0xBBu8; CHUNK as usize], 2 * CHUNK)
                .unwrap();
            f.sync_all().unwrap();
        }

        let base = BaseShm::open(&path, total).unwrap();
        assert!(base.is_populated(0, CHUNK), "pre-warmed chunk 0");
        assert!(base.is_populated(2 * CHUNK, CHUNK), "pre-warmed chunk 2");
        assert!(!base.is_populated(CHUNK, CHUNK), "hole at chunk 1");
        assert!(!base.is_populated(5 * CHUNK, CHUNK), "tail hole");
        assert!(
            !base.is_populated(0, 2 * CHUNK),
            "range spanning data+hole is not fully populated"
        );

        // Populate chunk 1 through the API: the map must update without
        // a re-scan (and coalesce with chunk 0's range).
        base.write_chunk(CHUNK, &vec![0xCCu8; CHUNK as usize])
            .unwrap();
        assert!(base.is_populated(CHUNK, CHUNK), "freshly written chunk 1");
        assert!(
            base.is_populated(0, 3 * CHUNK),
            "0..3 chunks now contiguous data (coalesced)"
        );

        // A disjoint later write inserts its own range.
        base.write_chunk(6 * CHUNK, &vec![0xDDu8; CHUNK as usize])
            .unwrap();
        assert!(base.is_populated(6 * CHUNK, CHUNK));
        assert!(
            !base.is_populated(5 * CHUNK, CHUNK),
            "hole before it remains"
        );
    }
}
