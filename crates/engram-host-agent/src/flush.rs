//! Cold-tier flush primitive (ADR 0005 / Stage 5).
//!
//! Given an Idle session whose hot snapshot lives at `snapshot_path`,
//! tar+zstd the directory, stream the result to a `BlobStorage`
//! backend, atomically flip the snapshot row + session status via
//! `MetadataStore::flush_to_cold`, and best-effort remove the local
//! directory. Returns a [`FlushOutcome`] for the caller to log /
//! surface to the admin endpoint.
//!
//! The pipeline is deliberately narrow:
//!
//! ```text
//!   tar -cf -    →    zstd -3 -T0    →    BlobStorage::put_streaming
//!   (no temp file; no in-memory buffering)
//! ```
//!
//! `tar` and `zstd` come from the host's `$PATH`. Both are tiny,
//! universally available, and well-understood — we'd rather lean on
//! them than carry the equivalent in-process code (in-process tar
//! exists in `tar` crate v0.4 but doesn't pipe cleanly to async
//! `BlobStorage` writers without staging).
//!
//! Idempotency: the underlying `MetadataStore::flush_to_cold` is
//! idempotent on already-cold rows (its `WHERE blob_present = FALSE`
//! guard). Two concurrent `flush_session` calls for the same session
//! both succeed; the second one's blob upload is wasted work but the
//! second `flush_to_cold` no-ops.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use engram_core::traits::{BlobStorage, MetadataStore, SealedBlobRef};
use engram_core::{HostId, MetaError, SessionId, SnapshotId};
use serde::{Deserialize, Serialize};

/// Result of a successful flush. Returned to the admin endpoint and
/// logged at info level.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlushOutcome {
    pub session_id: SessionId,
    pub snapshot_id: SnapshotId,
    pub blob_size_bytes: u64,
    pub took_ms: u64,
}

#[derive(Debug)]
pub enum FlushError {
    /// Pre-flight check failed: snapshot directory missing / empty.
    PreflightMissing(PathBuf),
    /// `tar | zstd` child process failed.
    Pipeline(String),
    /// Blob upload failed.
    Upload(String),
    /// Sealing the blob URL under the deployment KEK failed.
    Seal(String),
    /// Postgres update (flush_to_cold) failed.
    Meta(MetaError),
}

impl std::fmt::Display for FlushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PreflightMissing(p) => {
                write!(f, "snapshot path missing or empty: {}", p.display())
            }
            Self::Pipeline(m) => write!(f, "tar/zstd pipeline failed: {m}"),
            Self::Upload(m) => write!(f, "blob upload failed: {m}"),
            Self::Seal(m) => write!(f, "seal blob ref failed: {m}"),
            Self::Meta(e) => write!(f, "metadata update failed: {e}"),
        }
    }
}

impl std::error::Error for FlushError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Meta(e) => Some(e),
            _ => None,
        }
    }
}

/// Pluggable seal hook so the coordinator can wrap its KEK-backed
/// `seal_blob_ref` helper without the host-agent depending on the
/// coord crate. The host-agent passes a closure; the coord supplies
/// `engram_coordinator::blob::seal_blob_ref`.
pub type SealFn = Arc<
    dyn Fn(String) -> futures::future::BoxFuture<'static, Result<SealedBlobRef, String>>
        + Send
        + Sync,
>;

/// Inputs to [`flush_session`]. Callers (Stage 5's admin endpoint
/// and Stage 7's disk-pressure detector) build this struct, pass it
/// through.
pub struct FlushRequest {
    pub session_id: SessionId,
    pub snapshot_id: SnapshotId,
    pub host_id: HostId,
    pub snapshot_path: PathBuf,
}

/// Run the flush pipeline. Single function, no internal threading
/// concerns — concurrency caps live at the caller (admin's
/// `flush-idle` bounds parallelism per-host; Stage 7's detector
/// runs sequentially).
pub async fn flush_session(
    req: FlushRequest,
    blob: &Arc<dyn BlobStorage>,
    meta: &Arc<dyn MetadataStore>,
    seal: &SealFn,
) -> Result<FlushOutcome, FlushError> {
    let start = Instant::now();

    // 1. Pre-flight: refuse to package an empty / missing snapshot
    //    dir. Cheaper than discovering it mid-tar; gives a clean
    //    error.
    preflight(&req.snapshot_path).await?;

    // 2. Decide the deterministic blob key + seal it. Sealing first
    //    means we error out before touching the network if the KEK
    //    is misconfigured.
    let key = format!(
        "engram/snapshots/{}/{}.tar.zst",
        req.host_id, req.snapshot_id
    );
    let sealed = seal(key.clone()).await.map_err(FlushError::Seal)?;

    // 3. Spawn `sh -c 'tar -cf - -C <path> . | zstd -3 -T0'` and
    //    stream its stdout directly into BlobStorage::put_streaming.
    //    Letting the shell wire the pipe avoids the two-child
    //    fd-handoff dance; one child, one stdout pump.
    let (body, mut child) = spawn_pipeline(&req.snapshot_path)
        .await
        .map_err(FlushError::Pipeline)?;
    let body_size = blob
        .put_streaming(&key, body)
        .await
        .map_err(|e| FlushError::Upload(e.to_string()))?;

    // 4. Reap the child and surface non-zero exits as pipeline
    //    failures (the upload may have streamed a truncated body
    //    that BlobStorage treated as success otherwise).
    let status = child
        .wait()
        .await
        .map_err(|e| FlushError::Pipeline(format!("wait: {e}")))?;
    if !status.success() {
        return Err(FlushError::Pipeline(format!("tar|zstd exit {status}")));
    }

    // 5. Atomic Postgres update. Idempotent on already-cold rows.
    meta.flush_to_cold(req.session_id, req.snapshot_id, sealed, chrono::Utc::now())
        .await
        .map_err(FlushError::Meta)?;

    // 6. Best-effort local cleanup. Failure here doesn't roll back
    //    the cold flip — the cold copy is durable; the local dir is
    //    just disk pressure we can recover from later.
    if let Err(e) = tokio::fs::remove_dir_all(&req.snapshot_path).await {
        tracing::warn!(
            session_id = %req.session_id,
            path = %req.snapshot_path.display(),
            error = %e,
            "flush: best-effort remove_dir_all failed; cold copy is safe",
        );
    }

    let took_ms = start.elapsed().as_millis() as u64;
    tracing::info!(
        session_id = %req.session_id,
        snapshot_id = %req.snapshot_id,
        bytes = body_size,
        took_ms,
        key = %key,
        "flushed snapshot to cold tier",
    );
    Ok(FlushOutcome {
        session_id: req.session_id,
        snapshot_id: req.snapshot_id,
        blob_size_bytes: body_size,
        took_ms,
    })
}

async fn preflight(path: &Path) -> Result<(), FlushError> {
    let meta = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(_) => return Err(FlushError::PreflightMissing(path.to_path_buf())),
    };
    if !meta.is_dir() {
        return Err(FlushError::PreflightMissing(path.to_path_buf()));
    }
    // Empty dir = pointless flush; reject so callers get a clean
    // error rather than uploading a 32-byte tar.
    let mut entries = tokio::fs::read_dir(path)
        .await
        .map_err(|_| FlushError::PreflightMissing(path.to_path_buf()))?;
    if entries
        .next_entry()
        .await
        .map_err(|_| FlushError::PreflightMissing(path.to_path_buf()))?
        .is_none()
    {
        return Err(FlushError::PreflightMissing(path.to_path_buf()));
    }
    Ok(())
}

/// Spawn `sh -c 'tar -cf - -C <path> . | zstd -3 -T0'` and return
/// a `ByteStream` over its stdout + the child handle for `wait()`.
///
/// We let the shell wire the tar→zstd pipe: simpler than tokio's
/// two-child fd-handoff dance, and `sh` is universally present on
/// any host that can run firecracker / VZ. Path is single-quoted
/// against the shell so spaces / unusual chars don't break the
/// command (the host-agent's `work_dir` paths are operator-chosen
/// but defensively quoted anyway).
async fn spawn_pipeline(
    path: &Path,
) -> Result<(engram_core::traits::ByteStream, tokio::process::Child), String> {
    use std::process::Stdio;
    use tokio::process::Command;

    let path_str = path
        .to_str()
        .ok_or_else(|| format!("snapshot path not valid utf-8: {}", path.display()))?;
    // Reject single quotes / control chars in the path — they'd
    // break the shell quoting. work_dir paths are operator-set; if
    // someone managed to plant a `'` in a UUID-named subdirectory
    // we'd want to know.
    if path_str.contains('\'') || path_str.chars().any(|c| c.is_control()) {
        return Err(format!(
            "snapshot path contains shell-unsafe chars: {path_str}"
        ));
    }
    let cmd = format!("tar -cf - -C '{path_str}' . | zstd -3 -T0 -c");

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn sh: {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "child stdout missing".to_string())?;

    use futures::StreamExt;
    let reader = tokio_util::io::ReaderStream::new(stdout);
    let mapped = reader.map(|chunk| chunk.map_err(engram_core::error::BlobError::Io));
    let body = engram_core::traits::ByteStream::new(mapped);
    Ok((body, child))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::traits::SealedBlobRef;
    use std::sync::Arc;

    fn fake_seal() -> SealFn {
        Arc::new(|_url: String| {
            Box::pin(async move {
                Ok(SealedBlobRef {
                    wrapped_dek: vec![0u8; 32],
                    nonce: vec![0u8; 12],
                    ciphertext: vec![0u8; 16],
                    key_id: "test:v1".into(),
                })
            })
        })
    }

    #[tokio::test]
    async fn preflight_rejects_missing_dir() {
        let err = preflight(Path::new("/tmp/this-does-not-exist-engram-flush")).await;
        assert!(matches!(err, Err(FlushError::PreflightMissing(_))));
    }

    #[tokio::test]
    async fn preflight_rejects_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let err = preflight(dir.path()).await;
        assert!(matches!(err, Err(FlushError::PreflightMissing(_))));
    }

    #[tokio::test]
    async fn preflight_accepts_dir_with_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"hello").unwrap();
        preflight(dir.path()).await.unwrap();
    }

    #[test]
    fn flush_outcome_round_trips_through_serde() {
        let oc = FlushOutcome {
            session_id: SessionId::new(),
            snapshot_id: SnapshotId::new(),
            blob_size_bytes: 1234,
            took_ms: 56,
        };
        let json = serde_json::to_string(&oc).unwrap();
        let back: FlushOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back.blob_size_bytes, oc.blob_size_bytes);
    }

    // The seal closure is exercised from the seal/open round-trip in
    // engram-coordinator::blob; here we just lock down the type
    // shape so future refactors don't change the FlushFn signature
    // without realizing.
    #[tokio::test]
    async fn fake_seal_returns_a_sealed_ref() {
        let seal = fake_seal();
        let sealed = seal("engram/snapshots/foo/bar.tar.zst".into())
            .await
            .unwrap();
        assert_eq!(sealed.key_id, "test:v1");
    }
}
