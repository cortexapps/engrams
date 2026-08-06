//! ADR 0045 C2 (fork v3): the vmstate-only snapshot create primitive.
//!
//! The post-copy blackout writes only `state.bin` — the dirty guest memory
//! never materializes as a file (the destination demand-faults it from the
//! paused source's address space). Two arms:
//!
//! - **fork binary**: pause → `create_snapshot_vmstate_only` → `state.bin`
//!   written, no memory file appears, and the VM is still Paused afterwards
//!   (the method deliberately has no resume wrapper — the migration blackout
//!   owns pause/resume; an auto-resume mid-move is the split-brain).
//! - **stock binary**: the identical PUT is REJECTED (`deny_unknown_fields`
//!   on `CreateSnapshotParams`) — the fork-v3 capability gate that makes
//!   mixed-fleet rolls safe: a pre-roll host fails loudly at capture, before
//!   anything pauses for real in the orchestration (which then falls back).
//!
//! Same prereqs as `stock_fork_snapshot_compat`: `FC_TEST_KERNEL` /
//! `FC_TEST_ROOTFS` + `/dev/kvm` + `ENGRAM_FC_STOCK_BIN` / `ENGRAM_FC_FORK_BIN`.
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-sandbox-firecracker --test vmstate_only_snapshot -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::client::FirecrackerClient;
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, RestoreMode};

fn spec(label: &str, rootfs: PathBuf) -> SandboxSpec {
    SandboxSpec {
        image: label.into(),
        rootfs_source: Some(rootfs),
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
    }
}

fn make_cfg(env: &common::CompatEnv, bin: PathBuf) -> FirecrackerConfig {
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    cfg.net_pool = None; // unprivileged test — see lifecycle.rs
    cfg.firecracker_bin = bin;
    cfg.restore_mode = RestoreMode::File;
    // Public ubuntu rootfs has no /sbin/engram-init; boot to bash so the VM
    // survives the pre-snapshot sleep. See snapshot.rs.
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    cfg
}

#[tokio::test]
#[ignore = "needs /dev/kvm + FC test artifacts + stock/fork binaries"]
async fn vmstate_only_writes_state_skips_memory_and_stock_rejects_it() {
    let Some(env) = common::compat_preflight() else {
        return;
    };

    // ---- Fork arm: the primitive works and never touches memory. ----
    {
        let work = tempfile::tempdir().expect("tempdir");
        let local_rootfs = work.path().join("rootfs.ext4");
        common::clone_rootfs(&env.rootfs, &local_rootfs)
            .await
            .expect("clone rootfs");

        let backend = FirecrackerBackend::new(work.path(), make_cfg(&env, env.fork_bin.clone()));
        let vm = backend
            .create(spec("vmstate-only-fork", local_rootfs))
            .await
            .expect("create (fork)");
        // Poll the serial console (firecracker.log) for the kernel banner rather
        // than sleeping a fixed interval before snapshotting.
        let fc_log = work.path().join(vm.to_string()).join("firecracker.log");
        common::wait_for_log_contains(&fc_log, &["Linux version"], Duration::from_secs(15)).await;

        let st = backend.snapshot_state(vm).expect("vm state");
        let api = FirecrackerClient::new(&st.firecracker_socket);
        let out = tempfile::tempdir().expect("out dir");
        let state_path = out.path().join("state.bin");

        api.pause().await.expect("pause");
        api.create_snapshot_vmstate_only(&state_path)
            .await
            .expect("vmstate-only create on the fork binary");

        let state_len = std::fs::metadata(&state_path).expect("state.bin").len();
        assert!(state_len > 0, "state.bin is empty");
        // The memory leg was skipped: nothing else materialized in the out dir.
        let extras: Vec<_> = std::fs::read_dir(out.path())
            .expect("read out dir")
            .flatten()
            .filter(|e| e.file_name() != "state.bin")
            .map(|e| e.file_name())
            .collect();
        assert!(extras.is_empty(), "unexpected memory artifacts: {extras:?}");

        // No auto-resume happened: the VM is still Paused, so resume succeeds.
        api.resume().await.expect("VM should still be paused");

        backend.destroy(vm).await.expect("destroy (fork)");
    }

    // ---- Stock arm: the capability gate — stock FC rejects the field. ----
    {
        let work = tempfile::tempdir().expect("tempdir");
        let local_rootfs = work.path().join("rootfs.ext4");
        common::clone_rootfs(&env.rootfs, &local_rootfs)
            .await
            .expect("clone rootfs");

        let backend = FirecrackerBackend::new(work.path(), make_cfg(&env, env.stock_bin.clone()));
        let vm = backend
            .create(spec("vmstate-only-stock", local_rootfs))
            .await
            .expect("create (stock)");
        // Poll the serial console (firecracker.log) for the kernel banner rather
        // than sleeping a fixed interval before the snapshot PUT.
        let fc_log = work.path().join(vm.to_string()).join("firecracker.log");
        common::wait_for_log_contains(&fc_log, &["Linux version"], Duration::from_secs(15)).await;

        let st = backend.snapshot_state(vm).expect("vm state");
        let api = FirecrackerClient::new(&st.firecracker_socket);
        let out = tempfile::tempdir().expect("out dir");

        api.pause().await.expect("pause");
        let err = api
            .create_snapshot_vmstate_only(&out.path().join("state.bin"))
            .await
            .expect_err("stock FC must reject the vmstate_only field");
        eprintln!("stock rejection (expected): {err:?}");

        api.resume().await.expect("resume stock VM");
        backend.destroy(vm).await.expect("destroy (stock)");
    }
}
