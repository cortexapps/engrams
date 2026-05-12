//! Coordinator-side blob-storage selection.
//!
//! Picks the `BlobStorage` impl every chunk-store read/write routes
//! through. The ADR 0005 cold-tier seal/open/unpack helpers retired
//! with Phase 7 — chunks live in `BlobStorage` directly under
//! content-addressed keys (no envelope encryption needed since the
//! chunks themselves are inert bytes; security boundary is the
//! deployment KEK on registry credentials + session secrets, which
//! still use `engram-crypto::CredCipher`).

use std::sync::Arc;

use engram_core::traits::BlobStorage;

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
            "ENGRAM_BLOB_BACKEND=s3 is reserved for a follow-up; only `local` and `gcs` are supported today".into(),
        ),
        other => Err(format!(
            "unknown ENGRAM_BLOB_BACKEND={other}; expected `local` or `gcs`"
        )),
    }
}
