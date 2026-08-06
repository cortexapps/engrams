//! ADR 0014 M1.11 cross-host restore regression guard.
//!
//! This test snapshots a microVM in one `FirecrackerBackend` instance
//! and restores from the resulting `SnapshotMetadata` against a
//! **different** backend instance with a **different** `work_dir`.
//!
//! What it catches:
//!
//! Firecracker's `state.bin` embeds the absolute `path_on_host` the
//! source VM's `PUT /drives` used. On the source side that's
//! `<source_work_dir>/rootfs/<source_sandbox>.dev`. The receiver's
//! own `work_dir` is a different location, so without a fix the
//! receiver creates a symlink at its own canonical path and FC's
//! `load_snapshot` opens the source-embedded path and fails with
//! "Block: Virtio backend error: No such file or directory".
//!
//! `restore_canonical_symlinks` records `source_rootfs_canonical`
//! on the manifest at snapshot time and recreates the symlink at
//! the source path on restore — that's the contract this test
//! exercises end-to-end.
//!
//! Run via:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-sandbox-firecracker --test cross_host_restore \
//!     -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig};

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker; run with --ignored on the dev VM"]
async fn restore_succeeds_with_different_work_dir_than_source() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };

    // Two completely independent work dirs — simulates the prod
    // shape where a bake runner uses `tempfile::tempdir()` and a
    // production host uses `/var/lib/engram/sandboxes`. The
    // rootfs.ext4 lives in a third location (shared between both
    // sides) — the receiver materializes it locally in real prod;
    // here we just hand it the same path so the test stays narrow.
    let source_work = tempfile::tempdir().expect("source work_dir");
    let receiver_work = tempfile::tempdir().expect("receiver work_dir");
    let shared = tempfile::tempdir().expect("shared rootfs dir");

    let local_rootfs = shared.path().join("rootfs.ext4");
    common::clone_rootfs(&env.rootfs, &local_rootfs)
        .await
        .expect("clone rootfs");

    // Source-side backend: snapshots get state.bin pointing at
    // <source_work>/rootfs/<source_sandbox>.dev.
    let mut source_cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    source_cfg.net_pool = None;
    source_cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    let source = FirecrackerBackend::new(source_work.path(), source_cfg);

    let spec = SandboxSpec {
        image: "fc-cross-host-test".into(),
        rootfs_source: Some(local_rootfs.clone()),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 128 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
        swap_mib: None,
    };

    let source_id = source.create(spec).await.expect("create on source");
    // Wait for the kernel banner on the serial console rather than a fixed
    // settle before snapshotting.
    let _ = common::wait_for_log_contains(
        &source_work
            .path()
            .join(source_id.to_string())
            .join("firecracker.log"),
        &["Linux version"],
        Duration::from_secs(15),
    )
    .await;

    let metadata = source.snapshot(source_id).await.expect("snapshot");
    let source_snap_dir = source.snapshot_path_for(metadata.id);
    assert!(source_snap_dir.join("manifest.json").exists());

    // Verify the bake-side manifest stamped its canonical path.
    // The receiver needs this field to recreate the symlink
    // FC looks for at load_snapshot time.
    let manifest_json = source_snap_dir.join("manifest.json");
    let manifest_bytes = tokio::fs::read(&manifest_json).await.unwrap();
    let manifest_value: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
    let stamped_path = manifest_value
        .get("source_rootfs_canonical")
        .and_then(|v| v.as_str())
        .expect("source_rootfs_canonical must be stamped on the snapshot manifest");
    let expected_prefix = source_work.path().display().to_string();
    assert!(
        stamped_path.starts_with(&expected_prefix),
        "source_rootfs_canonical {stamped_path:?} should be under source work_dir {expected_prefix:?}",
    );

    source.destroy(source_id).await.expect("destroy source");

    // Receiver-side backend: distinct work_dir, never saw the
    // source's tempdir. Without the source_*_canonical plumbing
    // this restore would fail because FC's load_snapshot opens
    // the embedded source path and finds nothing.
    //
    // Copy state.bin + memory.bin + manifest.json into the
    // receiver's snapshot dir (the real coord materializer
    // downloads them from BlobStorage; here we shortcut by
    // copying directly).
    let receiver_snap_dir = receiver_work
        .path()
        .join("snapshots")
        .join(metadata.id.to_string());
    tokio::fs::create_dir_all(&receiver_snap_dir).await.unwrap();
    for f in ["state.bin", "memory.bin", "manifest.json"] {
        tokio::fs::copy(source_snap_dir.join(f), receiver_snap_dir.join(f))
            .await
            .unwrap();
    }

    let mut receiver_cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    receiver_cfg.net_pool = None;
    receiver_cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    let receiver = FirecrackerBackend::new(receiver_work.path(), receiver_cfg);

    let restored_id = receiver
        .restore(metadata.clone())
        .await
        .expect("cross-host restore should succeed");

    // The source-side canonical-path symlink should now exist on
    // the receiver too — that's the path FC's `load_snapshot`
    // opened. Verify it's a symlink pointing at the rootfs.
    let source_canonical = std::path::PathBuf::from(stamped_path);
    let meta = tokio::fs::symlink_metadata(&source_canonical)
        .await
        .expect("source canonical symlink should exist on receiver after restore");
    assert!(
        meta.is_symlink(),
        "source canonical {} must be a symlink",
        source_canonical.display()
    );

    receiver
        .destroy(restored_id)
        .await
        .expect("destroy restored");
}
