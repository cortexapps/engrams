//! Stock ↔ fork Firecracker snapshot compatibility (ADR 0045 Phase B, R2).
//!
//! The engrams fork adds the MAP_SHARED / `Msync` surface but deliberately
//! never touches `src/vmm/src/snapshot/` and never bumps `SNAPSHOT_VERSION`, so
//! a snapshot taken by stock FC must restore on the fork and vice-versa. That
//! byte-compatibility is what makes a mixed fleet safe during a `nodeAssetsImage`
//! roll (some hosts forked, some not). This test proves it end-to-end: create +
//! snapshot with one binary, restore with the other, both directions.
//!
//! Drives the high-level `SandboxBackend` trait (create → snapshot → restore),
//! with File-mode restore (a direct read of `memory.bin` — the most direct
//! exercise of the on-disk snapshot format). Both backends share one work dir so
//! the restoring binary resolves the creating binary's snapshot + rootfs.
//!
//! Needs `FC_TEST_KERNEL` / `FC_TEST_ROOTFS` + `/dev/kvm` (like the other FC
//! tests) plus BOTH binaries via `ENGRAM_FC_STOCK_BIN` / `ENGRAM_FC_FORK_BIN`:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! export ENGRAM_FC_STOCK_BIN=/path/to/stock/firecracker
//! export ENGRAM_FC_FORK_BIN=/path/to/forked/firecracker
//! cargo test -p engram-sandbox-firecracker --test stock_fork_snapshot_compat -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, RestoreMode};

/// Create + snapshot a microVM with `creator_bin`, then File-restore that exact
/// snapshot with `restorer_bin`. Both backends share `work` so the restorer
/// resolves the creator's snapshot dir + rootfs. Panics on any failure.
async fn round_trip(
    creator_bin: PathBuf,
    restorer_bin: PathBuf,
    env: &common::CompatEnv,
    label: &str,
) {
    // Rootfs lives in `work` (not a per-sandbox jail dir) so it survives
    // destroy() — FC stores the absolute drive path in state.bin and reopens it
    // at restore. See tests/snapshot.rs for the full rationale.
    let work = tempfile::tempdir().expect("tempdir");
    let local_rootfs = work.path().join("rootfs.ext4");
    common::clone_rootfs(&env.rootfs, &local_rootfs)
        .await
        .expect("clone rootfs into tempdir");

    let make_cfg = |bin: PathBuf| {
        let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
        cfg.net_pool = None; // unprivileged test — see lifecycle.rs
        cfg.firecracker_bin = bin;
        cfg.restore_mode = RestoreMode::File;
        // Public ubuntu rootfs has no /sbin/engram-init; boot to bash so the VM
        // survives the pre-snapshot sleep. See snapshot.rs.
        cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
        cfg
    };

    let creator = FirecrackerBackend::new(work.path(), make_cfg(creator_bin));
    let restorer = FirecrackerBackend::new(work.path(), make_cfg(restorer_bin));

    let spec = SandboxSpec {
        image: label.into(),
        rootfs_source: Some(local_rootfs),
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

    // Create + snapshot with the first binary.
    let original_id = creator
        .create(spec)
        .await
        .unwrap_or_else(|e| panic!("{label}: create: {e:?}"));
    // Wait for the source VM to reach early boot before snapshotting — poll the
    // serial console (funneled to firecracker.log) for the kernel banner instead
    // of sleeping a fixed worst-case interval.
    let fc_log = work
        .path()
        .join(original_id.to_string())
        .join("firecracker.log");
    common::wait_for_log_contains(&fc_log, &["Linux version"], Duration::from_secs(15)).await;
    let metadata = creator
        .snapshot(original_id)
        .await
        .unwrap_or_else(|e| panic!("{label}: snapshot: {e:?}"));
    let snap_dir = creator.snapshot_path_for(metadata.id);
    assert!(
        snap_dir.join("state.bin").exists(),
        "{label}: state.bin missing"
    );
    assert!(
        snap_dir.join("memory.bin").exists(),
        "{label}: memory.bin missing"
    );
    creator
        .destroy(original_id)
        .await
        .unwrap_or_else(|e| panic!("{label}: destroy original: {e:?}"));

    // Restore the very same snapshot with the OTHER binary. A wire-format skew
    // (the R2 risk) would surface here as a deserialize/magic/version error.
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");
    let restored_id = match restorer.restore(metadata).await {
        Ok(id) => id,
        Err(e) => {
            // Dump firecracker.log (carries the guest console) for any preserved jail.
            for entry in std::fs::read_dir(work.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let log = entry.path().join("firecracker.log");
                if log.exists() {
                    eprintln!(
                        "--- {} ---\n{}",
                        log.display(),
                        std::fs::read_to_string(&log).unwrap_or_default()
                    );
                }
            }
            panic!("{label}: cross-binary restore failed (R2 snapshot-compat regression?): {e:?}");
        }
    };

    let listed = restorer.list().await.expect("list after restore");
    assert_eq!(listed, vec![restored_id], "{label}: restored VM not listed");
    let st = restorer
        .snapshot_state(restored_id)
        .unwrap_or_else(|| panic!("{label}: state present after restore"));
    assert_eq!(
        st.spec.image, label,
        "{label}: spec.image carried through restore"
    );

    restorer
        .destroy(restored_id)
        .await
        .unwrap_or_else(|e| panic!("{label}: destroy restored: {e:?}"));
}

#[tokio::test]
#[ignore = "requires Linux + KVM + ENGRAM_FC_STOCK_BIN + ENGRAM_FC_FORK_BIN; run with --ignored on the dev VM"]
async fn stock_created_snapshot_restores_on_fork() {
    let env = match common::compat_preflight() {
        Some(e) => e,
        None => return,
    };
    round_trip(
        env.stock_bin.clone(),
        env.fork_bin.clone(),
        &env,
        "compat-stock-to-fork",
    )
    .await;
}

#[tokio::test]
#[ignore = "requires Linux + KVM + ENGRAM_FC_STOCK_BIN + ENGRAM_FC_FORK_BIN; run with --ignored on the dev VM"]
async fn fork_created_snapshot_restores_on_stock() {
    let env = match common::compat_preflight() {
        Some(e) => e,
        None => return,
    };
    round_trip(
        env.fork_bin.clone(),
        env.stock_bin.clone(),
        &env,
        "compat-fork-to-stock",
    )
    .await;
}
