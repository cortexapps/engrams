//! The durable-filesystem seam (ADR 0098 Phase 2).
//!
//! [`HostFs`] exposes the durable-op primitives at OPERATION granularity —
//! write / sync_file / rename / sync_dir / read / read_dir / remove_file —
//! so the host-internal simulator's crash injector can fail BETWEEN each
//! (the H5 composition contract: SimFs owns reachability at operation
//! granularity; the exhaustively-constructed byte-level torn states stay in
//! ADR 0099 H5's static tests).
//!
//! P1 only DEFINES this trait and its prod impl. `durable_record` and the
//! shutdown spool still call `tokio::fs` directly; rewiring them onto
//! [`HostFs`] is P3+/P5 work (the SimFs boundary lands with the flows that
//! cross it).

use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

/// Granular durable-filesystem operations. The prod impl ([`TokioFs`])
/// delegates to `tokio::fs`; the sim impl (in `engram-dst-host`) drives a
/// per-run tempdir and injects crashes between calls.
#[async_trait]
pub trait HostFs: Send + Sync {
    /// Write `bytes` to `path`, creating/truncating it. Does NOT fsync —
    /// callers pair this with [`sync_file`](HostFs::sync_file) at the
    /// durability boundary.
    async fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;

    /// fsync the file at `path` (data + metadata), so a following rename
    /// publishes complete bytes.
    async fn sync_file(&self, path: &Path) -> io::Result<()>;

    /// Rename `from` → `to` (atomic publish of the temp file's bytes).
    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// fsync the directory `dir` so a preceding rename's directory-entry
    /// update is itself crash-durable.
    async fn sync_dir(&self, dir: &Path) -> io::Result<()>;

    /// Read the whole file at `path`.
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>>;

    /// List the entries of `dir` (full paths). A missing dir is an error;
    /// callers that tolerate absence map it themselves.
    async fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>>;

    /// Remove the file at `path`. Idempotent-tolerant callers treat
    /// `NotFound` as success themselves.
    async fn remove_file(&self, path: &Path) -> io::Result<()>;
}

/// The production [`HostFs`]: `tokio::fs` delegation. The sync ops copy the
/// exact fsync discipline of `engram-host-agent::durable_record::persist`:
/// open the target read-only and `sync_all()` (valid for a directory on
/// both Linux and macOS).
#[derive(Debug, Clone, Default)]
pub struct TokioFs;

#[async_trait]
impl HostFs for TokioFs {
    async fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        tokio::fs::write(path, bytes).await
    }

    async fn sync_file(&self, path: &Path) -> io::Result<()> {
        let f = tokio::fs::OpenOptions::new().read(true).open(path).await?;
        f.sync_all().await
    }

    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        tokio::fs::rename(from, to).await
    }

    async fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        tokio::fs::File::open(dir).await?.sync_all().await
    }

    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        tokio::fs::read(path).await
    }

    async fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let mut rd = tokio::fs::read_dir(dir).await?;
        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
            out.push(entry.path());
        }
        Ok(out)
    }

    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        tokio::fs::remove_file(path).await
    }
}
