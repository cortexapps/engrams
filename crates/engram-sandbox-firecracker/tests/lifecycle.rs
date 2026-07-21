//! End-to-end SandboxBackend lifecycle test against a real Firecracker.
//! Drives the trait surface (`create` -> `list` -> `destroy`) without
//! reaching into `FirecrackerClient` directly, so this test is what
//! the coordinator's host-agent will actually exercise in production.
//!
//! Same gating as `tests/boot.rs` — Linux + KVM + firecracker on PATH +
//! cached test artifacts. Run via:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-sandbox-firecracker --test lifecycle -- --ignored --nocapture
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
async fn create_list_destroy_round_trip() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };

    // The test rootfs is shared across runs; copy it so we can attach
    // it read-write without polluting the cache. Same workaround the
    // Firecracker examples use.
    let work = tempfile::tempdir().expect("tempdir");
    let local_rootfs = work.path().join("rootfs.ext4");
    common::clone_rootfs(&env.rootfs, &local_rootfs)
        .await
        .expect("clone rootfs into tempdir");

    // Tests run unprivileged — disable per-VM TAP/iptables provisioning
    // (CAP_NET_ADMIN required) so the lifecycle round-trip exercises
    // FC's create/destroy contract without depending on root.
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = None;
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "fc-lifecycle-test".into(),
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
    };

    // create
    let id = backend.create(spec).await.expect("create");

    // list reflects the new sandbox
    let listed = backend.list().await.expect("list");
    assert_eq!(listed, vec![id], "expected exactly one live sandbox");

    // jail dir + socket + log file should all exist
    let jail = work.path().join(id.to_string());
    assert!(jail.is_dir(), "jail dir missing at {}", jail.display());
    assert!(jail.join("firecracker.sock").exists(), "API socket missing");
    assert!(
        jail.join("firecracker.log").exists(),
        "firecracker log missing"
    );

    // Snapshotted state surfaces the spec + paths the backend used
    let st = backend
        .snapshot_state(id)
        .expect("state present after create");
    assert_eq!(st.firecracker_socket, jail.join("firecracker.sock"));
    assert_eq!(st.rootfs_path, local_rootfs);

    // Give the kernel a moment to actually boot before tearing down.
    // Not strictly required for the trait contract — but it makes a
    // failed boot (kernel panic, etc.) visible in firecracker.log
    // before the dir gets removed below.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // destroy clears the sandbox and removes the jail dir
    backend.destroy(id).await.expect("destroy");
    assert!(
        backend.list().await.expect("list").is_empty(),
        "list must be empty after destroy",
    );
    assert!(
        !jail.exists(),
        "destroy must remove jail dir; still at {}",
        jail.display(),
    );

    // destroy is idempotent
    backend
        .destroy(id)
        .await
        .expect("destroy on unknown id is no-op");
}
