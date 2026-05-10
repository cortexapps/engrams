//! Cold-tier blob plumbing — backend selection + sealed-blob-ref
//! seal/open helpers.
//!
//! ADR 0005 reintroduced cold-tier durability. Per-row blob URLs land
//! in Postgres envelope-encrypted under the deployment KEK (same
//! shape as `registry_credentials` and `session_secrets`); these
//! helpers wrap `engram-crypto::CredCipher` so the rest of the
//! coordinator never has to think about the cipher detail.
//!
//! Key path layout: `engram/snapshots/<host_id>/<snapshot_id>.tar.zst`.
//! The `host_id` prefix lets ops grep for "what does host X own,"
//! and snapshot UUIDs are unique per row. The actual key string
//! lives plaintext only in the host-agent's transient memory + on
//! the wire to the upload SDK; what Postgres stores is the sealed
//! ref.

use std::sync::Arc;

use engram_core::traits::{BlobStorage, SealedBlobRef};

use crate::error::ApiError;

/// Pick a `BlobStorage` from `ENGRAM_BLOB_BACKEND`. `local` (default)
/// reads `ENGRAM_LOCAL_PATH` for the on-disk root; `gcs` requires
/// `ENGRAM_GCS_BUCKET` (and honors `STORAGE_EMULATOR_HOST` for
/// fake-gcs-server). Fails closed at startup on misconfiguration.
pub async fn from_env() -> Result<Arc<dyn BlobStorage>, String> {
    let backend = std::env::var("ENGRAM_BLOB_BACKEND")
        .unwrap_or_else(|_| "local".to_string())
        .to_lowercase();
    match backend.as_str() {
        "local" => {
            // Mirror the coordinator's `local_path` default; sit
            // under `<root>/blobs/` so the layout is obvious to
            // someone poking at `var/`.
            let root = std::env::var("ENGRAM_LOCAL_PATH")
                .unwrap_or_else(|_| "./var/engram".to_string());
            let blobs_dir = std::path::PathBuf::from(root).join("blobs");
            tracing::info!(path = %blobs_dir.display(), "blob backend: local");
            Ok(Arc::new(engram_storage_local::LocalBlobStorage::new(
                blobs_dir,
            )))
        }
        "gcs" => {
            let bucket = std::env::var("ENGRAM_GCS_BUCKET").map_err(|_| {
                "ENGRAM_BLOB_BACKEND=gcs requires ENGRAM_GCS_BUCKET".to_string()
            })?;
            tracing::info!(bucket = %bucket, "blob backend: gcs");
            let store = engram_storage_gcs::GcsBlobStorage::connect(bucket)
                .await
                .map_err(|e| format!("gcs connect: {e}"))?;
            Ok(Arc::new(store))
        }
        "s3" => Err(
            "ENGRAM_BLOB_BACKEND=s3 is reserved for a follow-up; only `local` and `gcs` ship in Stage 4".into(),
        ),
        other => Err(format!(
            "unknown ENGRAM_BLOB_BACKEND={other}; expected `local` or `gcs`"
        )),
    }
}

/// Seal a plaintext blob URL under the deployment KEK. The result
/// is the four-column quartet that `flush_to_cold` writes to the
/// `snapshots` row.
///
/// Stage 5's flush primitive is the first caller; Stage 4 ships the
/// helper in advance so the seal/open pair is exercised by tests
/// before the production caller lands.
#[allow(dead_code)]
pub(crate) async fn seal_blob_ref(
    state: &crate::state::SharedState,
    plaintext_url: &str,
) -> Result<SealedBlobRef, ApiError> {
    let cipher = engram_crypto::CredCipher::new(state.services.kek.as_ref());
    let sealed = cipher
        .seal(plaintext_url.as_bytes())
        .await
        .map_err(|e| ApiError::Internal(format!("seal blob ref: {e}")))?;
    Ok(SealedBlobRef {
        wrapped_dek: sealed.wrapped_dek,
        nonce: sealed.nonce.to_vec(),
        ciphertext: sealed.ciphertext,
        key_id: sealed.key_id,
    })
}

/// Reverse of [`seal_blob_ref`]: open a sealed ref and return the
/// plaintext URL. Used by the cold-resume path (Stage 6) to drive
/// `Request::CopyBlobToLocal` against the picked host.
#[allow(dead_code)]
pub(crate) async fn open_blob_ref(
    state: &crate::state::SharedState,
    sealed: &SealedBlobRef,
) -> Result<String, ApiError> {
    let nonce: [u8; 12] = sealed
        .nonce
        .as_slice()
        .try_into()
        .map_err(|_| ApiError::Internal("sealed blob ref nonce wrong length".into()))?;
    let cred = engram_crypto::SealedCred {
        wrapped_dek: sealed.wrapped_dek.clone(),
        nonce,
        ciphertext: sealed.ciphertext.clone(),
        key_id: sealed.key_id.clone(),
    };
    let cipher = engram_crypto::CredCipher::new(state.services.kek.as_ref());
    let plaintext = cipher
        .open(&cred)
        .await
        .map_err(|e| ApiError::Internal(format!("open blob ref: {e}")))?;
    String::from_utf8(plaintext)
        .map_err(|e| ApiError::Internal(format!("blob ref not valid utf-8: {e}")))
}

/// Deterministic key for a snapshot's blob. Plaintext form; the
/// sealed version of this string is what lands in Postgres.
pub fn snapshot_blob_key(
    host_id: engram_core::HostId,
    snapshot_id: engram_core::SnapshotId,
) -> String {
    format!("engram/snapshots/{host_id}/{snapshot_id}.tar.zst")
}

/// Stream a blob out of `BlobStorage`, pipe through
/// `zstd -d | tar -xf -`, materialize the contents at `dest_dir`.
/// Reverse of the host-agent's flush-side pipeline; same `sh -c`
/// shell-pipe shape so the system dependencies match.
///
/// The destination directory is created if it doesn't exist. The
/// caller picks a unique path — typically
/// `<local_path>/snapshots/<session_id>/<new_uuid>/` so concurrent
/// resumes don't collide.
///
/// Returns the number of compressed bytes streamed in (= the blob's
/// size_bytes), useful for telemetry. The decompressed footprint
/// lives at `dest_dir`.
pub(crate) async fn unpack_blob_to_dir(
    blob: &std::sync::Arc<dyn BlobStorage>,
    key: &str,
    dest_dir: &std::path::Path,
) -> Result<u64, ApiError> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    tokio::fs::create_dir_all(dest_dir)
        .await
        .map_err(|e| ApiError::Internal(format!("create dest dir: {e}")))?;
    let dest_str = dest_dir
        .to_str()
        .ok_or_else(|| ApiError::Internal("dest path not valid utf-8".into()))?;
    if dest_str.contains('\'') || dest_str.chars().any(|c| c.is_control()) {
        return Err(ApiError::Internal(format!(
            "dest path contains shell-unsafe chars: {dest_str}"
        )));
    }

    let cmd = format!("zstd -d -c | tar -xf - -C '{dest_str}'");
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ApiError::Internal(format!("spawn unpack pipeline: {e}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| ApiError::Internal("unpack child stdin missing".into()))?;

    use futures::StreamExt;
    let mut stream = blob
        .get_streaming(key)
        .await
        .map_err(|e| ApiError::Internal(format!("blob get_streaming: {e}")))?;
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| ApiError::Internal(format!("blob stream: {e}")))?;
        total += bytes.len() as u64;
        stdin
            .write_all(&bytes)
            .await
            .map_err(|e| ApiError::Internal(format!("unpack stdin write: {e}")))?;
    }
    drop(stdin);

    let status = child
        .wait()
        .await
        .map_err(|e| ApiError::Internal(format!("unpack wait: {e}")))?;
    if !status.success() {
        return Err(ApiError::Internal(format!(
            "zstd|tar exit {status} during unpack"
        )));
    }
    tracing::debug!(
        key = %key,
        dest = %dest_dir.display(),
        bytes = total,
        "blob unpacked",
    );
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_blob_key_uses_host_then_snapshot() {
        let host = engram_core::HostId::new();
        let snap = engram_core::SnapshotId::new();
        let key = snapshot_blob_key(host, snap);
        assert!(key.starts_with("engram/snapshots/"));
        assert!(key.contains(&host.to_string()));
        assert!(key.ends_with(".tar.zst"));
    }

    #[tokio::test]
    async fn seal_then_open_round_trips_blob_url() {
        // Round-trip a blob URL through the same KEK-backed cipher
        // path that production uses. Locks down the seal/open pair
        // before Stage 5 wires it into the flush primitive.
        let kek: std::sync::Arc<dyn engram_crypto::MasterKeyProvider> = std::sync::Arc::new(
            engram_crypto::EnvVarKeyProvider::from_bytes([7u8; 32], "test:v1"),
        );
        let plaintext = "engram/snapshots/abc/123.tar.zst";
        let cipher = engram_crypto::CredCipher::new(kek.as_ref());
        let sealed = cipher.seal(plaintext.as_bytes()).await.unwrap();
        let sealed_ref = SealedBlobRef {
            wrapped_dek: sealed.wrapped_dek,
            nonce: sealed.nonce.to_vec(),
            ciphertext: sealed.ciphertext,
            key_id: sealed.key_id,
        };
        // Re-open via the same direct path the helper uses; locks
        // down the wrapped_dek/nonce/ciphertext shape.
        let nonce: [u8; 12] = sealed_ref.nonce.as_slice().try_into().unwrap();
        let cred = engram_crypto::SealedCred {
            wrapped_dek: sealed_ref.wrapped_dek,
            nonce,
            ciphertext: sealed_ref.ciphertext,
            key_id: sealed_ref.key_id,
        };
        let opened = cipher.open(&cred).await.unwrap();
        assert_eq!(String::from_utf8(opened).unwrap(), plaintext);
    }
}
