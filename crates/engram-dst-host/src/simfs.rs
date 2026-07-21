//! The per-run simulated filesystem (ADR 0098 Phase 2, P2).
//!
//! [`SimFs`] owns ONE tempdir root for a whole simulation run and hands out
//! the durable-operation subtrees the portable host-agent machinery writes
//! into: `records/` (durable_record's fsync-rename-fsync-parent contract),
//! `finalize/` (eviction_finalize), `spool/` (the shutdown spool), plus the
//! chunk-store blob dir and the chunk-cache dir that stand in for GCS + the
//! host page cache.
//!
//! **SimFs is the tempdir owner, NOT the interceptor.** Since ADR 0098 P5
//! the shipped `durable_record`/`spool` bodies issue every durable op
//! through the injected [`HostFs`](engram_host_core::HostFs) seam, and the
//! crash injector is [`CrashFs`](crate::CrashFs) — a real `HostFs` impl
//! that cuts at an op index, so the crash-point schedule is DERIVED from
//! the production op sequence by running it (`tests/crashpoint_coverage.rs`
//! pins the derivation). The P2-era `CrashPoint` boundary catalogue and the
//! externally-constructed post-crash states it mapped to are retired —
//! byte-level torn states remain ADR 0099 H5's static tests.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

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
