//! ADR 0035: host-side bundle generation store.
//!
//! Bundle generations are content-addressed files
//! (`<drive_id>-<sha256>.squashfs`) staged under the fleet-canonical
//! bundle dir. The FC-host image bakes the *current* generation; every
//! other generation a snapshot pins arrives here by being
//! **materialized** from BlobStorage (`bundles/sha256/<sha>`), and gets
//! there in the first place by being **published** at snapshot time —
//! so blob storage only ever holds generations some snapshot
//! referenced, which is exactly the GC's pin universe.
//!
//! Publish runs on *every* snapshot with aux refs (idempotent
//! HEAD-then-put): base captures publish the enable-time generation,
//! and eviction snapshots publish whatever the VM actually has attached
//! — which after a fresh-create swap (ADR 0035 §3) can be a baked
//! generation no capture ever published.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engram_core::traits::storage::BlobStorage;
use engram_core::types::sandbox::{AuxBundleRef, AuxRoDrive};
use engram_core::SandboxError;
use futures::StreamExt;
use sha2::{Digest, Sha256};

/// Resolve the staged-bundle dir: `ENGRAM_BUNDLE_DIR` env override
/// (dev / tests), else the fleet-canonical [`AuxRoDrive::SHARED_DIR`].
pub fn bundle_dir_from_env() -> PathBuf {
    std::env::var("ENGRAM_BUNDLE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(AuxRoDrive::SHARED_DIR))
}

/// Read the bake-time `current.json` stamp as bundle refs. Missing or
/// malformed stamp → empty + warn — a dev host without staged bundles
/// must still heartbeat (it just reports no current bundles, and any
/// capture requesting aux drives fails loudly at the FC layer).
pub async fn read_stamp(dir: &Path) -> Vec<AuxBundleRef> {
    let path = dir.join(AuxRoDrive::CURRENT_STAMP);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                stamp = %path.display(),
                error = %e,
                "no bundle stamp; reporting no current bundles",
            );
            return Vec::new();
        }
    };
    match serde_json::from_slice::<std::collections::HashMap<String, String>>(&bytes) {
        Ok(map) => {
            let mut refs: Vec<AuxBundleRef> = map
                .into_iter()
                .map(|(drive_id, sha256)| AuxBundleRef { drive_id, sha256 })
                .collect();
            refs.sort_by(|a, b| a.drive_id.cmp(&b.drive_id));
            refs
        }
        Err(e) => {
            tracing::warn!(stamp = %path.display(), error = %e, "malformed bundle stamp");
            Vec::new()
        }
    }
}

/// ADR 0035 §5: heartbeat-ack-driven bundle supervisor. Watches the
/// coord's `live_bundles` pin set and (a) prefetches any pinned
/// generation this host is missing — so resumes land warm instead of
/// paying the materialize on the restore path — and (b) deletes staged
/// generations that are neither pinned nor in the bake stamp ("the
/// bundle path is pinned by GC", host edition). Mirrors the
/// `image_prefetch` supervisor's watch-channel shape.
pub fn spawn_supervisor(
    store: BundleStore,
    current: Vec<AuxBundleRef>,
) -> tokio::sync::watch::Sender<Vec<AuxBundleRef>> {
    let (tx, mut rx) = tokio::sync::watch::channel(Vec::<AuxBundleRef>::new());
    tokio::spawn(async move {
        loop {
            if rx.changed().await.is_err() {
                tracing::debug!("bundle supervisor: live_bundles sender dropped; exiting");
                return;
            }
            let live = rx.borrow_and_update().clone();
            if let Err(e) = store.materialize_if_missing(&live).await {
                tracing::warn!(error = %e, "bundle prefetch failed; retrying on next ack change");
            }
            store.sweep_unpinned(&live, &current).await;
        }
    });
    tx
}

/// Publish + materialize operations over the staged-bundle dir and
/// BlobStorage. Held by `PooledBackend` when the host has a blob
/// backend wired (production always; tests opt in).
pub struct BundleStore {
    blob: Arc<dyn BlobStorage>,
    dir: PathBuf,
}

impl BundleStore {
    pub fn new(blob: Arc<dyn BlobStorage>, dir: PathBuf) -> Self {
        Self { blob, dir }
    }

    pub fn staged_path(&self, r: &AuxBundleRef) -> PathBuf {
        self.dir
            .join(AuxRoDrive::staged_file_name(&r.drive_id, &r.sha256))
    }

    /// Idempotently publish each referenced generation's bytes to
    /// BlobStorage. Errors are load-bearing: a snapshot whose pinned
    /// bundle isn't durable in blob storage can't be restored on
    /// another host, so the snapshot pipeline must fail (and retry)
    /// rather than record a pin nothing can satisfy.
    pub async fn publish(&self, refs: &[AuxBundleRef]) -> Result<(), SandboxError> {
        for r in refs {
            let key = AuxRoDrive::blob_key(&r.sha256);
            let exists =
                self.blob.exists(&key).await.map_err(|e| {
                    SandboxError::Snapshot(format!("bundle publish: HEAD {key}: {e}"))
                })?;
            if exists {
                continue;
            }
            let staged = self.staged_path(r);
            let file = tokio::fs::File::open(&staged).await.map_err(|e| {
                SandboxError::Snapshot(format!(
                    "bundle publish: open staged {}: {e}",
                    staged.display()
                ))
            })?;
            let stream = engram_core::traits::storage::ByteStream::new(
                tokio_util::io::ReaderStream::new(file)
                    .map(|r| r.map_err(engram_core::error::BlobError::Io)),
            );
            let size =
                self.blob.put_streaming(&key, stream).await.map_err(|e| {
                    SandboxError::Snapshot(format!("bundle publish: put {key}: {e}"))
                })?;
            tracing::info!(
                drive_id = %r.drive_id,
                sha256 = %r.sha256,
                size_bytes = size,
                "bundle generation published to BlobStorage (first reference)",
            );
        }
        Ok(())
    }

    /// Ensure every pinned generation is staged locally, fetching from
    /// BlobStorage (with digest verification + atomic rename) when
    /// missing. Called before `inner.restore*` so FC's `load_snapshot`
    /// finds the embedded path, and by the heartbeat-driven prefetch.
    pub async fn materialize_if_missing(&self, refs: &[AuxBundleRef]) -> Result<(), SandboxError> {
        for r in refs {
            let staged = self.staged_path(r);
            if tokio::fs::try_exists(&staged).await.unwrap_or(false) {
                continue;
            }
            self.fetch_one(r, &staged).await?;
        }
        Ok(())
    }

    /// Delete staged generations that are neither in the coord's pin
    /// set nor in the bake stamp. Best-effort (a failed unlink is just
    /// disk not reclaimed); deleting a file an FC VM still has open is
    /// safe (unlinked-but-open) — the pin set covers every snapshot, so
    /// nothing that needs *re-opening* is ever swept.
    pub async fn sweep_unpinned(&self, live: &[AuxBundleRef], current: &[AuxBundleRef]) {
        let keep: std::collections::HashSet<String> = live
            .iter()
            .chain(current.iter())
            .map(|r| AuxRoDrive::staged_file_name(&r.drive_id, &r.sha256))
            .collect();
        let mut dir = match tokio::fs::read_dir(&self.dir).await {
            Ok(d) => d,
            Err(_) => return, // no staging dir — nothing to sweep
        };
        while let Ok(Some(entry)) = dir.next_entry().await {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // Only files matching the staged-generation shape; the
            // stamp itself and any temp files stay.
            if !name.ends_with(".squashfs") || keep.contains(name) {
                continue;
            }
            match tokio::fs::remove_file(entry.path()).await {
                Ok(()) => {
                    tracing::info!(file = %name, "swept unpinned bundle generation");
                }
                Err(e) => {
                    tracing::warn!(file = %name, error = %e, "bundle sweep: unlink failed");
                }
            }
        }
    }

    async fn fetch_one(&self, r: &AuxBundleRef, staged: &Path) -> Result<(), SandboxError> {
        let key = AuxRoDrive::blob_key(&r.sha256);
        let started = std::time::Instant::now();
        let mut stream = self.blob.get_streaming(&key).await.map_err(|e| {
            SandboxError::Snapshot(format!(
                "bundle materialize: get {key}: {e} — the pinned generation is \
                 neither staged on this host nor in BlobStorage (GC bug or a \
                 snapshot recorded without publish)"
            ))
        })?;
        if let Some(parent) = staged.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("bundle materialize: mkdir: {e}")))?;
        }
        // Same-dir temp file so the final rename is atomic on the same
        // filesystem; suffix by pid to keep concurrent restores of the
        // same generation from clobbering each other mid-write (last
        // rename wins; both bodies are identical by construction).
        let tmp = staged.with_extension(format!("tmp.{}", std::process::id()));
        let mut file = tokio::fs::File::create(&tmp).await.map_err(|e| {
            SandboxError::Snapshot(format!("bundle materialize: create {}: {e}", tmp.display()))
        })?;
        let mut hasher = Sha256::new();
        let mut size: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                SandboxError::Snapshot(format!("bundle materialize: read {key}: {e}"))
            })?;
            hasher.update(&chunk);
            size += chunk.len() as u64;
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("bundle materialize: write: {e}")))?;
        }
        tokio::io::AsyncWriteExt::flush(&mut file)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("bundle materialize: flush: {e}")))?;
        drop(file);
        let got = format!("{:x}", hasher.finalize());
        if got != r.sha256 {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(SandboxError::Snapshot(format!(
                "bundle materialize: digest mismatch for {key}: got {got}, \
                 want {} — refusing to stage corrupt bytes",
                r.sha256
            )));
        }
        tokio::fs::rename(&tmp, staged).await.map_err(|e| {
            SandboxError::Snapshot(format!(
                "bundle materialize: rename {} -> {}: {e}",
                tmp.display(),
                staged.display()
            ))
        })?;
        tracing::info!(
            drive_id = %r.drive_id,
            sha256 = %r.sha256,
            size_bytes = size,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "bundle generation materialized from BlobStorage",
        );
        Ok(())
    }
}
