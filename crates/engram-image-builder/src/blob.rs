//! `BlobStorage` selection for the image-builder CLI.
//!
//! Mirrors `engram_coordinator::blob::from_env` and
//! `engram_host_agent::blob::from_env` so production CI bakes can
//! land chunks directly in the deployment's GCS bucket without
//! needing an intermediate local-to-GCS upload step.
//!
//! In dev (the default), chunks fall back to the local-filesystem
//! root at `<images_dir>/store/` — same layout the dev workflow
//! has always used. Production sets `ENGRAM_BLOB_BACKEND=gcs` +
//! `ENGRAM_GCS_BUCKET=<bucket>` in the CI runner's environment.

use std::path::Path;
use std::sync::Arc;

use engram_core::traits::BlobStorage;

/// Pick a `BlobStorage` from `ENGRAM_BLOB_BACKEND`.
///
/// - `local` (default) — `<images_dir>/store/` on the CI runner's
///   filesystem. Used by `just bake` and dev workflows.
/// - `gcs` — production target. Requires `ENGRAM_GCS_BUCKET`.
///   The `images_dir` argument is ignored for the chunk root in
///   this mode; chunks land at the bucket's root keyspace
///   (`chunks/sha256/...` per `ChunkHash::storage_key`).
///   Honors `STORAGE_EMULATOR_HOST` for fake-gcs-server.
///
/// Fails closed on misconfiguration — a bake that can't reach its
/// chunk backend should refuse rather than silently produce a
/// half-baked artifact that depends on a bucket that doesn't exist.
pub async fn from_env(images_dir: &Path) -> Result<Arc<dyn BlobStorage>, String> {
    let backend = std::env::var("ENGRAM_BLOB_BACKEND")
        .unwrap_or_else(|_| "local".to_string())
        .to_lowercase();
    match backend.as_str() {
        "local" => {
            let chunk_root = images_dir.join("store");
            tokio::fs::create_dir_all(&chunk_root)
                .await
                .map_err(|e| format!("create chunk root {}: {e}", chunk_root.display()))?;
            tracing::info!(path = %chunk_root.display(), "blob backend: local");
            Ok(Arc::new(engram_storage_local::LocalBlobStorage::new(
                chunk_root,
            )))
        }
        "gcs" => {
            let bucket = std::env::var("ENGRAM_GCS_BUCKET")
                .map_err(|_| "ENGRAM_BLOB_BACKEND=gcs requires ENGRAM_GCS_BUCKET".to_string())?;
            tracing::info!(bucket = %bucket, "blob backend: gcs");
            let store = engram_storage_gcs::GcsBlobStorage::connect(bucket)
                .await
                .map_err(|e| format!("gcs connect: {e}"))?;
            Ok(Arc::new(store))
        }
        "s3" => Err(
            "ENGRAM_BLOB_BACKEND=s3 is reserved; only `local` and `gcs` are supported today".into(),
        ),
        other => Err(format!(
            "unknown ENGRAM_BLOB_BACKEND={other}; expected `local` or `gcs`"
        )),
    }
}
