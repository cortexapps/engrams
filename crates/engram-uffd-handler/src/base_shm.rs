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
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Is `[offset, offset+len)` fully populated (no holes)? A cheap
    /// `lseek(SEEK_HOLE)` probe: if the first hole at-or-after `offset`
    /// sits at-or-past the range end, every page in range is present in
    /// the page cache and `UFFDIO_CONTINUE` can map it without a fetch.
    pub fn is_populated(&self, offset: u64, len: u64) -> bool {
        let end = offset.saturating_add(len).min(self.total_bytes);
        // SAFETY: plain lseek on an fd we own; no memory is touched.
        let hole = unsafe {
            libc::lseek64(
                self.file.as_raw_fd(),
                offset as libc::off64_t,
                libc::SEEK_HOLE,
            )
        };
        if hole < 0 {
            // ENXIO = offset past EOF (shouldn't happen — sized at open);
            // any error ⇒ treat as unpopulated and let pwrite repair.
            return false;
        }
        (hole as u64) >= end
    }

    /// Write canonical chunk bytes at `offset`. Idempotent across
    /// concurrent sibling handlers (same bytes, same offset).
    pub fn write_chunk(&self, offset: u64, bytes: &[u8]) -> Result<(), BaseShmError> {
        self.file
            .write_all_at(bytes, offset)
            .map_err(|source| BaseShmError::Write { offset, source })
    }
}
