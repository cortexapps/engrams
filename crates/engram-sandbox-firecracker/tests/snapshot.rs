//! End-to-end snapshot/restore round-trip test against a real Firecracker.
//! Drives the SandboxBackend trait through `create` → `snapshot` →
//! `destroy` → `restore` → `destroy`. Same gating as the other ignored
//! integration tests (Linux + KVM + firecracker on PATH + cached test
//! artifacts).
//!
//! Run via:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-sandbox-firecracker --test snapshot -- --ignored --nocapture
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
async fn snapshot_then_restore_round_trips_microvm() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };

    // Rootfs lives in `work` (NOT in a per-sandbox jail dir) so it
    // survives `destroy()` of the original sandbox — Firecracker stores
    // the absolute drive path in state.bin and reopens it on
    // load_snapshot, so the file must still be there at restore time.
    let work = tempfile::tempdir().expect("tempdir");
    let local_rootfs = work.path().join("rootfs.ext4");
    tokio::fs::copy(&env.rootfs, &local_rootfs)
        .await
        .expect("clone rootfs into tempdir");

    // The default `with_kernel` config boots into `/sbin/engram-init`,
    // which only exists in our agent-baked images. The public
    // ubuntu-22.04 ext4 rootfs we fetch has no such init, so the
    // guest kernel panics 1–2s into boot — which is exactly when
    // the test reaches `snapshot()` and discovers firecracker is
    // dead. Override the init to a binary that's actually present
    // in the rootfs so the VM stays alive for the snapshot pause.
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    // Unprivileged test — see lifecycle.rs comment.
    cfg.net_pool = None;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "fc-snapshot-test".into(),
        rootfs_source: Some(local_rootfs.clone()),
        image_uri: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 128 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
    };

    // Step 1: create
    let original_id = backend.create(spec).await.expect("create");

    // Step 2: let the kernel finish early-boot before snapshotting.
    // Snapshotting too early can capture a half-initialised state
    // that hangs on resume; 2s is conservative for a static rootfs
    // and keeps the test fast.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Step 3: snapshot (ADR 0007 Phase 6: backend owns staging)
    let metadata = backend.snapshot(original_id).await.expect("snapshot");
    let snap_dir = backend.snapshot_path_for(metadata.id);

    assert!(snap_dir.join("state.bin").exists(), "state.bin missing");
    assert!(snap_dir.join("memory.bin").exists(), "memory.bin missing");
    assert!(
        snap_dir.join("manifest.json").exists(),
        "manifest.json missing"
    );
    assert_eq!(metadata.image_version, "fc-snapshot-test");
    // memory.bin alone should be ~ guest RAM (128 MiB). Lower bound
    // catches a regression where we accidentally truncate the file.
    assert!(
        metadata.size_bytes >= 100 * 1024 * 1024,
        "snapshot size suspiciously small: {} bytes",
        metadata.size_bytes,
    );

    // Step 4: original VM should still be live after snapshot —
    // create_snapshot pauses + resumes; it doesn't tear the VM down.
    let listed = backend.list().await.expect("list");
    assert_eq!(listed, vec![original_id], "original VM gone after snapshot");

    // Step 5: destroy the original
    backend.destroy(original_id).await.expect("destroy");
    assert!(
        backend.list().await.expect("list").is_empty(),
        "list should be empty after destroying the original",
    );

    // Step 6: restore from the snapshot — gets a *new* sandbox id
    let restored_id = backend.restore(metadata.clone()).await.expect("restore");
    assert_ne!(
        restored_id, original_id,
        "restore must allocate a fresh sandbox id"
    );

    // Step 7: restored VM is in the list and has the right paths
    let listed = backend.list().await.expect("list after restore");
    assert_eq!(listed, vec![restored_id]);

    let st = backend
        .snapshot_state(restored_id)
        .expect("state present after restore");
    let restored_jail = work.path().join(restored_id.to_string());
    assert_eq!(
        st.firecracker_socket,
        restored_jail.join("firecracker.sock")
    );
    assert_eq!(
        st.spec.image, "fc-snapshot-test",
        "manifest's spec.image carried through restore"
    );
    assert_eq!(st.rootfs_path, local_rootfs, "rootfs path preserved");

    // Step 8: cleanup
    backend
        .destroy(restored_id)
        .await
        .expect("destroy restored");
}
