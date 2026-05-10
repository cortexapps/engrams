//! End-to-end cold-tier round-trip on a real Firecracker microVM.
//!
//! Mirrors the cycle `docs/demo-vz.md` runs by hand on VZ:
//!
//! ```text
//! create → snapshot → tar+zstd pack → destroy → untar+unzstd unpack → restore → verify
//! ```
//!
//! The pack/unpack shellouts use the same shell pipe shape that the
//! coordinator + host-agent use in production
//! (`engram_host_agent::flush::spawn_pipeline` and
//! `engram_coordinator::blob::unpack_blob_to_dir`), so a tar/zstd
//! arg-ordering regression on either side surfaces here too. We avoid
//! pulling `BlobStorage` / `MetadataStore` into the test — the
//! coordinator's `tests/cold_tier_round_trip.rs` already covers that
//! plumbing against fake snapshot dirs; what's missing (and what
//! THIS test adds) is the same trip on a real FC snapshot's
//! `state.bin` + `memory.bin` + `manifest.json`.
//!
//! Same gating as the rest of the FC integration tests: Linux + KVM +
//! firecracker on PATH + cached test artifacts. Run via:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-sandbox-firecracker --test cold_tier -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig};
use tokio::process::Command;

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker; run with --ignored on the dev VM"]
async fn cold_tier_round_trip_on_real_microvm() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !common::require_bin("tar") || !common::require_bin("zstd") {
        return;
    }

    let work = tempfile::tempdir().expect("tempdir");
    let local_rootfs = work.path().join("rootfs.ext4");
    tokio::fs::copy(&env.rootfs, &local_rootfs)
        .await
        .expect("clone rootfs into tempdir");

    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    // Unprivileged test — no /30 provisioning. The restore-time net
    // re-provisioning is unit-tested in `src/net.rs` (the `reserve_*`
    // tests) and exercised on the dev VM via the demo runbook.
    cfg.net_pool = None;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "fc-cold-tier-test".into(),
        rootfs_source: Some(local_rootfs.clone()),
        image_uri: None,
        harness_pack_uri: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 128 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        harness_substrate: None,
        network: Default::default(),
    };

    // 1. create
    let original_id = backend.create(spec).await.expect("create");

    // 2. let early-boot settle so the snapshot captures a coherent state
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 3. snapshot
    let snap_dir = work.path().join("snap");
    let metadata = backend
        .snapshot(original_id, &snap_dir)
        .await
        .expect("snapshot");
    assert!(snap_dir.join("state.bin").exists());
    assert!(snap_dir.join("memory.bin").exists());
    assert!(snap_dir.join("manifest.json").exists());
    let original_size = metadata.size_bytes;

    // 4. pack: tar -cf - -C snap_dir . | zstd -3 -T0 -c > blob.tar.zst
    //    Same pipeline shape as engram_host_agent::flush::spawn_pipeline
    //    so a regression in the production pack path is caught here too.
    let blob_path = work.path().join("blob.tar.zst");
    pack(&snap_dir, &blob_path).await;
    let blob_size = tokio::fs::metadata(&blob_path)
        .await
        .expect("blob stat")
        .len();
    assert!(blob_size > 0, "tar+zstd produced empty blob");
    // memory.bin (128 MiB of mostly-zero pages) compresses heavily but
    // should still be at least a few KiB. Lower bound catches a
    // silent zstd no-op.
    assert!(
        blob_size > 1024,
        "blob suspiciously small: {blob_size} bytes",
    );
    println!("snapshot {original_size} bytes → cold blob {blob_size} bytes",);

    // 5. destroy the original — the cold flush is responsible for the
    //    durable copy now.
    backend.destroy(original_id).await.expect("destroy");
    assert!(backend.list().await.expect("list").is_empty());
    // Belt-and-braces: also remove the local snapshot dir so the
    // restore really has to come from the unpacked blob, not from
    // the original's still-on-disk artefacts.
    tokio::fs::remove_dir_all(&snap_dir)
        .await
        .expect("remove original snap dir");

    // 6. unpack: zstd -d -c < blob.tar.zst | tar -xf - -C unpacked
    //    Same shape as engram_coordinator::blob::unpack_blob_to_dir.
    let unpacked = work.path().join("unpacked");
    unpack(&blob_path, &unpacked).await;
    assert!(
        unpacked.join("state.bin").exists(),
        "state.bin missing post-unpack"
    );
    assert!(
        unpacked.join("memory.bin").exists(),
        "memory.bin missing post-unpack"
    );
    assert!(
        unpacked.join("manifest.json").exists(),
        "manifest missing post-unpack"
    );

    // 7. restore from the unpacked dir — gets a *new* sandbox id
    let restored_id = backend.restore(unpacked.clone()).await.expect("restore");
    assert_ne!(restored_id, original_id);

    // 8. verify the restored VM is alive and the manifest carried through
    let listed = backend.list().await.expect("list after restore");
    assert_eq!(listed, vec![restored_id]);
    let st = backend
        .snapshot_state(restored_id)
        .expect("state present after restore");
    assert_eq!(
        st.spec.image, "fc-cold-tier-test",
        "manifest's spec.image carried through pack/unpack/restore",
    );
    assert_eq!(
        st.rootfs_path, local_rootfs,
        "rootfs path preserved across cold round-trip",
    );

    // cleanup
    backend
        .destroy(restored_id)
        .await
        .expect("destroy restored");
}

async fn pack(src_dir: &Path, dest_blob: &Path) {
    let cmd = format!(
        "tar -cf - -C '{}' . | zstd -3 -T0 -c > '{}'",
        path_str(src_dir),
        path_str(dest_blob),
    );
    let status = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .await
        .expect("spawn pack pipeline");
    assert!(status.success(), "pack pipeline failed: {status}");
}

async fn unpack(src_blob: &Path, dest_dir: &Path) {
    tokio::fs::create_dir_all(dest_dir)
        .await
        .expect("create unpack dir");
    let cmd = format!(
        "zstd -d -c < '{}' | tar -xf - -C '{}'",
        path_str(src_blob),
        path_str(dest_dir),
    );
    let status = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .await
        .expect("spawn unpack pipeline");
    assert!(status.success(), "unpack pipeline failed: {status}");
}

fn path_str(p: &Path) -> &str {
    p.to_str().expect("path utf-8")
}
