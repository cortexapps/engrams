//! The per-run simulated filesystem (ADR 0098 Phase 2, P2).
//!
//! [`SimFs`] owns ONE tempdir root for a whole simulation run and hands out
//! the durable-operation subtrees the portable host-agent machinery writes
//! into: `records/` (durable_record's fsync-rename-fsync-parent contract),
//! `finalize/` (eviction_finalize), `spool/` (the shutdown spool), plus the
//! chunk-store blob dir and the chunk-cache dir that stand in for GCS + the
//! host page cache.
//!
//! **SimFs is the tempdir owner + boundary bookkeeper, NOT a syscall
//! interceptor.** The shipped `durable_record`/`spool` code keeps calling
//! `tokio::fs` unchanged; the production [`TokioFs`](engram_host_core::TokioFs)
//! is what the [`HostEffects`](engram_host_core::HostEffects) bundle wires,
//! rooted at these directories. What SimFs adds in P2 is the [`CrashPoint`]
//! enum — the reachable durable-operation boundaries a future crash injector
//! (P4) will cut at — and the reachability plumbing that names them. **No
//! scheduler-driven crash INJECTION happens in P2**; wiring the boundary
//! catalogue now lets P4 cut at a named point without re-deriving the map,
//! and lets `tests/crashpoint_coverage.rs` prove every boundary already has
//! an ADR 0099 H5 externally-constructed-state test standing behind it.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

/// A durable-operation boundary a crash can land between. These are the
/// exact `tokio::fs` steps of the two durable formats P2 drives:
///
/// * `durable_record::persist` — write the `.json.partial` temp, fsync it,
///   rename it over the destination, fsync the parent dir; and
/// * `spool::write_spool` — write+fsync each `chunk-*.bin`, write+fsync the
///   `meta.json` completeness marker LAST, fsync the directory entries.
///
/// The variant order is the write order within each format. P4's crash
/// injector will select one of these and drop the process immediately
/// after the boundary; P2 only *names* them (the enum + the coverage
/// meta-test) — the enum is `#[non_exhaustive]`-free ON PURPOSE so a new
/// boundary is a compile error in [`crate::CrashPoint::ALL`] and in the
/// coverage table, never a silent gap (the AGENTS.md exhaustiveness-guard
/// pattern).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CrashPoint {
    // ── durable_record::persist legs ────────────────────────────────
    /// After the `.json.partial` temp is written, before its fsync — a
    /// torn/short temp file (never yet the destination).
    PersistWritePartial,
    /// After the temp fsync, before the rename — a complete `.json.partial`
    /// sits beside the (old or absent) destination.
    PersistFsyncTemp,
    /// After the rename, before the parent-dir fsync — the destination
    /// entry exists in the page cache but its directory block may not be
    /// durable yet.
    PersistRename,
    /// After the parent-dir fsync — the fully durable record. (The
    /// happy-path anchor; a crash here loses nothing.)
    PersistFsyncParent,

    // ── spool::write_spool legs ─────────────────────────────────────
    /// Mid `chunk-*.bin` writes: some chunk files present (possibly torn),
    /// no completeness marker yet.
    SpoolChunks,
    /// A listed chunk file went missing after being written — the
    /// missing-file leg of the marker's all-or-nothing contract.
    SpoolChunkMissing,
    /// Before/mid the `meta.json` marker write — no marker, or a torn one:
    /// the spool reads as absent (no marker) or is rejected (torn marker).
    SpoolMarker,
    /// After the marker fsync, before the directory-entry fsync — the
    /// marker's bytes are durable but the dir block naming it may not be;
    /// the zero-chunk ref-only spool exercises this leg.
    SpoolDir,
}

impl CrashPoint {
    /// Every boundary, in write order. A new [`CrashPoint`] variant that is
    /// not added here is a compile error at the array literal — and the
    /// coverage meta-test iterates `ALL`, so it can never silently escape
    /// the H5-state mapping.
    pub const ALL: [CrashPoint; 8] = [
        CrashPoint::PersistWritePartial,
        CrashPoint::PersistFsyncTemp,
        CrashPoint::PersistRename,
        CrashPoint::PersistFsyncParent,
        CrashPoint::SpoolChunks,
        CrashPoint::SpoolChunkMissing,
        CrashPoint::SpoolMarker,
        CrashPoint::SpoolDir,
    ];

    /// Which durable format this boundary belongs to (`"persist"` or
    /// `"spool"`). Wildcard-free so a new variant forces a decision here.
    pub fn format(self) -> &'static str {
        match self {
            CrashPoint::PersistWritePartial
            | CrashPoint::PersistFsyncTemp
            | CrashPoint::PersistRename
            | CrashPoint::PersistFsyncParent => "persist",
            CrashPoint::SpoolChunks
            | CrashPoint::SpoolChunkMissing
            | CrashPoint::SpoolMarker
            | CrashPoint::SpoolDir => "spool",
        }
    }
}

/// The per-run filesystem root. Dropped at end-of-run, which deletes the
/// tempdir. A [`CrashProcess`](crate::Step::CrashProcess) does NOT drop
/// this — the disk survives a process crash; only the RAM backends die.
pub struct SimFs {
    root: TempDir,
    records: PathBuf,
    finalize: PathBuf,
    spool: PathBuf,
    chunks: PathBuf,
    cache: PathBuf,
}

impl SimFs {
    /// Create the run's tempdir and its durable subtrees. The dir names are
    /// stable so a failure trace is comparable; the tempdir's random PREFIX
    /// never feeds a decision or appears in the trace (determinism rule).
    pub fn new() -> std::io::Result<Self> {
        let root = tempfile::tempdir()?;
        let base = root.path();
        let records = base.join("records");
        let finalize = base.join("finalize");
        let spool = base.join("spool");
        let chunks = base.join("chunks");
        let cache = base.join("cache");
        for d in [&records, &finalize, &spool, &chunks, &cache] {
            std::fs::create_dir_all(d)?;
        }
        Ok(Self {
            root,
            records,
            finalize,
            spool,
            chunks,
            cache,
        })
    }

    /// The tempdir root (for wiring [`HostEffects`](engram_host_core::HostEffects)).
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// `records/` — durable_record's `persist`/`load_all` directory.
    pub fn records_dir(&self) -> &Path {
        &self.records
    }

    /// `finalize/` — eviction_finalize's durable records.
    pub fn finalize_dir(&self) -> &Path {
        &self.finalize
    }

    /// `spool/` — the shutdown-spool root (`<spool>/<sandbox_id>/…`).
    pub fn spool_dir(&self) -> &Path {
        &self.spool
    }

    /// The chunk-store blob dir — the GCS stand-in (durable across crashes).
    pub fn chunks_dir(&self) -> &Path {
        &self.chunks
    }

    /// The chunk-cache root; per-sandbox subdirs live under it.
    pub fn cache_dir(&self) -> &Path {
        &self.cache
    }
}
