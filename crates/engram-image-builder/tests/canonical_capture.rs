//! Bake-time canonical memory capture integration test.
//!
//! Linux + KVM + FC + the test rootfs are required: the test
//! re-uses `FirecrackerBackend` to boot a transient VM with the
//! cached rootfs, snapshots `memory.bin` after a fixed boot wait,
//! and chunks it into a chunk store. Validates that
//! `Baker::capture_canonical_memory` returns a `ManifestRef`
//! pointing at the captured memory bytes.
//!
//! Pairs with the existing FC integration suite — same
//! `FC_TEST_KERNEL` / `FC_TEST_ROOTFS` env vars and the same
//! `/dev/kvm` requirement.
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-image-builder \
//!     --test canonical_capture \
//!     -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use engram_chunk_store::{ChunkStore, ManifestKind};
use engram_core::traits::BlobStorage;
use engram_image_builder::{Builder, CanonicalCaptureConfig, Mke2fsPacker};
use engram_storage_local::LocalBlobStorage;

fn preflight() -> Option<(PathBuf, PathBuf)> {
    let kernel = match std::env::var("FC_TEST_KERNEL") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: FC_TEST_KERNEL not set");
            return None;
        }
    };
    let rootfs = match std::env::var("FC_TEST_ROOTFS") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: FC_TEST_ROOTFS not set");
            return None;
        }
    };
    if !std::path::Path::new("/dev/kvm").exists() {
        eprintln!("SKIP: /dev/kvm not present");
        return None;
    }
    if std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|p| p.join("firecracker"))
                .find(|p| p.is_file())
        })
        .is_none()
    {
        eprintln!("SKIP: firecracker binary not on PATH");
        return None;
    }
    Some((kernel, rootfs))
}

// Trivial Docker stub — `capture_canonical_memory` doesn't go
// through the docker build path at all; we instantiate the
// builder with a stub so we can call the method directly.
#[derive(Clone)]
struct NoopDocker;
#[async_trait::async_trait]
impl engram_image_builder::DockerRunner for NoopDocker {
    async fn build(
        &self,
        _args: engram_image_builder::docker::BuildArgs,
    ) -> Result<(), engram_image_builder::docker::DockerError> {
        Err(engram_image_builder::docker::DockerError::NotFound)
    }
    async fn create(
        &self,
        _tag: &str,
    ) -> Result<String, engram_image_builder::docker::DockerError> {
        Err(engram_image_builder::docker::DockerError::NotFound)
    }
    async fn export_to_dir(
        &self,
        _container_id: &str,
        _dest: &std::path::Path,
    ) -> Result<(), engram_image_builder::docker::DockerError> {
        Err(engram_image_builder::docker::DockerError::NotFound)
    }
    async fn rm_container(
        &self,
        _id: &str,
    ) -> Result<(), engram_image_builder::docker::DockerError> {
        Ok(())
    }
    async fn rmi(&self, _tag: &str) -> Result<(), engram_image_builder::docker::DockerError> {
        Ok(())
    }
    fn clone_runner(&self) -> Box<dyn engram_image_builder::DockerRunner> {
        Box::new(self.clone())
    }
}

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + test rootfs"]
async fn capture_canonical_memory_chunks_post_boot_memory() {
    let (kernel, rootfs) = match preflight() {
        Some(v) => v,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
    let store = ChunkStore::new(blob);

    let builder = Builder::with_packer(NoopDocker, Mke2fsPacker::default(), store.clone());

    let capture_cfg = CanonicalCaptureConfig {
        kernel_image_path: kernel,
        firecracker_bin: None, // PATH-resolved
        // Public ubuntu-22.04 rootfs panics quickly when init=bash
        // and there's no shell-driven runtime; 3 s is long enough
        // for the kernel to settle in steady state before the FC
        // pause races. Production bakes with sentinels use longer
        // wait + readiness detection.
        boot_wait: Duration::from_secs(3),
        memory_mib: Some(128),
        // Skip the M1.14 profile pass in this fixture; the
        // chunked-restore plumbing needs prod-shaped wiring.
        uffd_handler_bin: None,
        blob_root: None,
        // The Ubuntu rootfs this test uses has no engram-init and
        // no engram-bootstrap; opt out of the M1.12 stub-harness +
        // engram-init scaffolding so the kernel can reach a shell.
        skip_warm_pool_prep: true,
    };

    // ADR 0014 M1.11: `capture_canonical_memory` now stages
    // state.bin + sidecar into `image_dir` (so push_to_registry
    // can layer them into OCI). Tests don't push — just point at
    // any writable tempdir and ignore the staged files.
    let stage = tempfile::tempdir().expect("stage tempdir");
    let snapshot_metadata = match builder
        .capture_canonical_memory(&rootfs, stage.path(), &capture_cfg)
        .await
    {
        Ok(r) => r,
        Err(e) => panic!("capture_canonical_memory failed: {e}"),
    };

    // ADR 0014 M1.3 changed the return type from `ManifestRef` to
    // `SnapshotMetadata`. The chunked memory manifest is one field
    // inside it; state.bin / sidecar live alongside.
    let manifest_ref = snapshot_metadata
        .memory_manifest
        .expect("canonical capture must populate memory_manifest");

    // The manifest is in the store; size + chunk count are non-trivial.
    let manifest = store.get_manifest(manifest_ref).await.unwrap();
    assert!(matches!(manifest.kind, ManifestKind::Memory));
    assert!(
        manifest.total_bytes > 0,
        "canonical memory must have content"
    );

    // ADR 0014 M1.11: state.bin + sidecar are now staged into
    // image_dir for push_to_registry to layer into OCI. Verify
    // both exist and are non-empty — the OCI roundtrip relies on
    // them being readable from this exact path.
    let staged_state = stage.path().join("snapshot.state.bin");
    let staged_sidecar = stage.path().join("snapshot.sidecar.json");
    assert!(
        staged_state.exists(),
        "state.bin must be staged at {}",
        staged_state.display()
    );
    assert!(
        staged_sidecar.exists(),
        "sidecar must be staged at {}",
        staged_sidecar.display()
    );
    assert!(
        tokio::fs::metadata(&staged_state).await.unwrap().len() > 0,
        "staged state.bin must be non-empty"
    );
    assert!(
        tokio::fs::metadata(&staged_sidecar).await.unwrap().len() > 0,
        "staged sidecar must be non-empty"
    );
    // Coord assigns the blob keys at enable-image; bake leaves
    // them None now.
    assert!(snapshot_metadata.state_blob_key.is_none());
    assert!(snapshot_metadata.sidecar_blob_key.is_none());

    // Materialize it back to verify chunks are reachable.
    let recovered = tmp.path().join("recovered-canonical.bin");
    store
        .materialize_to_file(&manifest, &recovered)
        .await
        .unwrap();
    let meta = tokio::fs::metadata(&recovered).await.unwrap();
    assert_eq!(meta.len(), manifest.total_bytes);

    eprintln!(
        "CAPTURED: canonical memory manifest {} ({} chunks, {} MiB)",
        manifest_ref,
        manifest.chunks.len(),
        manifest.total_bytes / (1024 * 1024)
    );
}
