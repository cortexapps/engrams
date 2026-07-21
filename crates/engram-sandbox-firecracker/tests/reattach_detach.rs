//! ADR 0044 K2: VM-detach + live-reattach across a host-agent "restart".
//!
//! Proves the core K2 invariant against a real Firecracker: a microVM's
//! lifecycle is decoupled from the host-agent process. We
//!
//!   1. `create()` a VM on one backend (= host-agent generation A),
//!   2. drop that backend WITHOUT destroying the sandbox (= generation A
//!      exits — SIGTERM detach, no kill, no checkpoint), and assert the
//!      FC process is *still alive* (this is what removing `kill_on_drop`
//!      buys us),
//!   3. build a fresh backend on the same work_dir (= generation B) and
//!      `reattach_sandbox()` off the on-disk `sandbox.json`, asserting the
//!      VM rejoins `list()`, then
//!   4. `destroy()` the reattached sandbox — which owns no `Child`, so it
//!      tears down purely by recorded pid — and assert the FC is gone.
//!
//! Same gating as `tests/lifecycle.rs` — Linux + KVM + firecracker on
//! PATH + cached test artifacts. Run via:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-sandbox-firecracker --test reattach_detach -- --ignored --nocapture
//! ```
//!
//! Runs unprivileged (`net_pool = None`), so it exercises the cold /
//! host-root reattach path (pidfd + three-axis identity + FC API
//! liveness). The per-VM netns reattach branch needs CAP_NET_ADMIN and
//! is validated on the dev VM.

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::sandbox_manifest;
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig};

/// True iff `pid` names a live (non-zombie) process. Reads
/// `/proc/<pid>/stat`: the state char follows the last `)`; `Z` is a
/// reaped-but-not-collected zombie, which we count as dead.
fn fc_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s
            .rsplit(')')
            .next()
            .and_then(|rest| rest.trim_start().chars().next())
            .map(|state| state != 'Z')
            .unwrap_or(false),
        Err(_) => false,
    }
}

fn test_spec(rootfs: std::path::PathBuf) -> SandboxSpec {
    SandboxSpec {
        image: "fc-reattach-test".into(),
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
    }
}

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker; run with --ignored on the dev VM"]
async fn detached_vm_survives_backend_drop_and_reattaches() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };

    // Shared work_dir across both backend generations — the successor
    // reads the predecessor's `sandbox.json` from here.
    let work = tempfile::tempdir().expect("tempdir");
    let local_rootfs = work.path().join("rootfs.ext4");
    common::clone_rootfs(&env.rootfs, &local_rootfs)
        .await
        .expect("clone rootfs into tempdir");

    let mk_cfg = || {
        let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
        // Unprivileged: no per-VM TAP/netns (needs CAP_NET_ADMIN). The
        // reattach path under test is the cold / host-root one.
        cfg.net_pool = None;
        cfg
    };

    // --- Generation A: create the VM ---
    let backend_a = FirecrackerBackend::new(work.path(), mk_cfg());
    let id = backend_a
        .create(test_spec(local_rootfs.clone()))
        .await
        .expect("create");
    assert_eq!(backend_a.list().await.expect("list"), vec![id]);

    // The manifest the reattach pass will read, plus the FC pid it
    // records. create() must have written it (ADR 0044 K2).
    let manifest_path = sandbox_manifest::manifest_path(work.path(), id);
    let manifest = sandbox_manifest::read_manifest(&manifest_path)
        .expect("create() must persist sandbox.json for reattach");
    let fc_pid = manifest.firecracker.process.pid;
    assert!(fc_alive(fc_pid), "FC should be running right after create");

    // Let the guest actually boot so FC's API socket is serving when the
    // successor probes it for liveness.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // --- Generation A exits: drop the backend WITHOUT destroying ---
    // This is the detach: a host-agent restart drops its in-memory state
    // but must NOT kill the node's VMs. With `kill_on_drop` removed, the
    // dropped `Child` does not SIGKILL FC.
    drop(backend_a);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        fc_alive(fc_pid),
        "FC must survive the host-agent backend being dropped (detach)"
    );

    // The old host-agent's vsock listeners died with it. Model that by
    // removing the harness UDS (in-process, generation A's listener task
    // lingers; a real process restart takes it down). Only a successful
    // re-bind on reattach makes it connectable again — without that, the
    // guest harness adapter re-dials forever and the session wedges.
    let harness_uds =
        engram_sandbox_firecracker::harness_uds_for(&manifest.firecracker.vsock_uds_base);
    let _ = std::fs::remove_file(&harness_uds);
    assert!(
        tokio::net::UnixStream::connect(&harness_uds).await.is_err(),
        "harness listener must be gone once the old host-agent exits"
    );

    // --- Generation B: reattach off the persisted manifest ---
    let backend_b = FirecrackerBackend::new(work.path(), mk_cfg());
    assert!(
        backend_b.list().await.expect("list").is_empty(),
        "fresh backend starts with no sandboxes until it reattaches"
    );
    backend_b
        .reattach_sandbox(&manifest)
        .await
        .expect("reattach the still-live FC");
    assert_eq!(
        backend_b.list().await.expect("list"),
        vec![id],
        "reattached sandbox must rejoin list() so the first heartbeat re-advertises it"
    );
    assert!(fc_alive(fc_pid), "reattach must not disturb the running FC");

    // ADR 0044 K2: reattach must re-bind the host-side harness vsock
    // listener, or the guest adapter can never reconnect and the run
    // pauses forever. (forge + upload listeners are re-bound alongside.)
    tokio::net::UnixStream::connect(&harness_uds)
        .await
        .expect("reattach must re-bind the harness vsock listener");

    // --- Teardown by pid: the reattached sandbox owns no `Child` ---
    backend_b
        .destroy(id)
        .await
        .expect("destroy reattached sandbox");
    assert!(
        backend_b.list().await.expect("list").is_empty(),
        "list empty after destroying the reattached sandbox"
    );
    assert!(
        !fc_alive(fc_pid),
        "destroy must SIGKILL the FC by recorded pid even with no owned Child"
    );
}
