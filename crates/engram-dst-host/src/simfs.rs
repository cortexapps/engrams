//! The per-run simulated filesystem (ADR 0098 Phase 2, P2).
//!
//! [`SimFs`] owns one temporary root for a simulation run. It provides the
//! durable-operation subtrees that the host-agent uses. `records/` contains
//! durable records. `finalize/` contains eviction finalize records. `dirty/`
//! contains the per-sandbox dirty files. The chunk store and chunk cache
//! directories represent GCS and the host page cache.
//!
//! `SimFs` owns the temporary directory. It does not intercept file calls.
//! The durable record code uses the injected
//! [`HostFs`](engram_host_core::HostFs) seam. [`CrashFs`](crate::CrashFs)
//! cuts that operation sequence at a selected index.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

/// The per-run filesystem root. Dropped at end-of-run, which deletes the
/// temporary directory. A [`CrashProcess`](crate::Step::CrashProcess) does
/// not drop this value. The disk survives process death.
pub struct SimFs {
    root: TempDir,
    records: PathBuf,
    finalize: PathBuf,
    dirty: PathBuf,
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
        let dirty = base.join("dirty");
        let chunks = base.join("chunks");
        let cache = base.join("cache");
        for d in [&records, &finalize, &dirty, &chunks, &cache] {
            std::fs::create_dir_all(d)?;
        }
        Ok(Self {
            root,
            records,
            finalize,
            dirty,
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

    /// The root for stable per-sandbox dirty files.
    pub fn dirty_dir(&self) -> &Path {
        &self.dirty
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
