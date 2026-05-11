//! Host-agent-side `BlobStorage` selection.
//!
//! Mirrors `engram_coordinator::blob::from_env` for the multi-host
//! topology: the standalone `engram-host-agent` binary needs its own
//! BlobStorage Arc to back the ADR 0007 chunk store. In a real GCP
//! deployment the coordinator and every host-agent point at the
//! same GCS bucket, so chunks the bake step (or another host's
//! snapshot) wrote are readable here.
//!
//! Keep this module narrow: only backend selection lives here. The
//! coordinator's `blob.rs` also carries seal/unseal helpers for the
//! envelope-encrypted snapshot pipeline — those don't belong on the
//! host-agent side (no KEK reach), and they're retiring with Phase 7.

use std::sync::Arc;

use engram_core::traits::BlobStorage;

/// Pick a `BlobStorage` from `ENGRAM_BLOB_BACKEND`.
///
/// - `local` (default) — `<root>/blobs/` on the host's filesystem,
///   where `<root>` is `ENGRAM_LOCAL_PATH` (default `./var/engram`).
///   Useful for single-machine dev where coordinator + host-agent
///   share a working directory.
/// - `gcs` — production target. Requires `ENGRAM_GCS_BUCKET`.
///   Honors `STORAGE_EMULATOR_HOST` for fake-gcs-server in test setups.
///
/// Fails closed at startup on misconfiguration — a host-agent that
/// can't reach its chunk backend should refuse to dial home, not
/// pretend to be ready and fail every session create.
pub async fn from_env() -> Result<Arc<dyn BlobStorage>, String> {
    let backend = std::env::var("ENGRAM_BLOB_BACKEND")
        .unwrap_or_else(|_| "local".to_string())
        .to_lowercase();
    match backend.as_str() {
        "local" => {
            let root =
                std::env::var("ENGRAM_LOCAL_PATH").unwrap_or_else(|_| "./var/engram".to_string());
            let blobs_dir = std::path::PathBuf::from(root).join("blobs");
            tracing::info!(path = %blobs_dir.display(), "blob backend: local");
            Ok(Arc::new(engram_storage_local::LocalBlobStorage::new(
                blobs_dir,
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex;

    // `from_env` reads process-global env. Run the cases inside a
    // single test under one lock so we don't race when nextest fans
    // out, and restore the env on the way out so a test failure
    // doesn't poison neighbors with leaked state. `tokio::sync::Mutex`
    // (not `std::sync::Mutex`) so the guard can be held across the
    // `.await` calls below without tripping `clippy::await_holding_lock`.
    static ENV_LOCK: Mutex<()> = Mutex::const_new(());

    fn set_env(key: &str, value: Option<&str>) -> Option<String> {
        let prev = std::env::var(key).ok();
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        prev
    }

    fn restore_env(key: &str, prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[tokio::test]
    async fn from_env_dispatches_per_backend_var() {
        let _guard = ENV_LOCK.lock().await;

        // Case 1: unset → defaults to local under `<root>/blobs`.
        {
            let p1 = set_env("ENGRAM_BLOB_BACKEND", None);
            let p2 = set_env("ENGRAM_LOCAL_PATH", Some("/tmp/engram-blob-test"));
            assert!(from_env().await.is_ok(), "default-local should succeed",);
            restore_env("ENGRAM_BLOB_BACKEND", p1);
            restore_env("ENGRAM_LOCAL_PATH", p2);
        }

        // Case 2: gcs without bucket → clear error naming the var.
        {
            let p1 = set_env("ENGRAM_BLOB_BACKEND", Some("gcs"));
            let p2 = set_env("ENGRAM_GCS_BUCKET", None);
            let err = match from_env().await {
                Ok(_) => panic!("missing bucket must error"),
                Err(e) => e,
            };
            assert!(
                err.contains("ENGRAM_GCS_BUCKET"),
                "error must name the missing var: {err}",
            );
            restore_env("ENGRAM_BLOB_BACKEND", p1);
            restore_env("ENGRAM_GCS_BUCKET", p2);
        }

        // Case 3: unknown backend → error names the bad value.
        {
            let p = set_env("ENGRAM_BLOB_BACKEND", Some("azure"));
            let err = match from_env().await {
                Ok(_) => panic!("unknown backend must error"),
                Err(e) => e,
            };
            assert!(
                err.contains("azure"),
                "error must echo the unknown value: {err}",
            );
            restore_env("ENGRAM_BLOB_BACKEND", p);
        }

        // Case 4: s3 → explicit "reserved" message, not just a generic
        //   "unknown". The next person tempted to deploy on AWS sees
        //   "s3 isn't supported yet" before they get further.
        {
            let p = set_env("ENGRAM_BLOB_BACKEND", Some("s3"));
            let err = match from_env().await {
                Ok(_) => panic!("s3 stub must surface as a clear error"),
                Err(e) => e,
            };
            assert!(
                err.contains("s3"),
                "s3 error must call out s3 explicitly: {err}",
            );
            restore_env("ENGRAM_BLOB_BACKEND", p);
        }
    }
}
