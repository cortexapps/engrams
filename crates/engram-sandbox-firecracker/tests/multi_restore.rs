//! ADR 0014 M1.10: serial multi-restore correctness gate, plus the
//! concurrent same-snapshot regression for the vsock re-key.
//!
//! The warm-pool refill loop's steady-state shape: lease a slot,
//! destroy it (consumed by a session), refill creates a new slot
//! from the same template snapshot. The serial test exercises that
//! cycle by:
//!
//!   - Creating + snapshotting + destroying a source sandbox.
//!   - Restoring N times in sequence from the same snapshot,
//!     destroying each restored microVM before the next restore.
//!
//! ### The concurrent test (vsock UDS re-key regression)
//!
//! Historically N concurrent restores from one snapshot collided on
//! the source-sandbox-id-keyed vsock UDS path (FC's state.bin embeds
//! it; every descendant bound the SAME absolute path, and the last
//! binder silently stole exec/harness traffic from live siblings —
//! the 2026-06-11 cross-session misroute). Restores now pass the
//! fork's `vsock_override` keyed to the new live sandbox id, so
//! same-snapshot VMs coexist on one host. The concurrent test pins
//! that: all N alive at once, each with its OWN id-keyed UDS bound
//! by a live FC, and destroying one VM must not disturb a sibling's
//! socket.
//!
//! Same gating as the other ignored FC integration tests
//! (Linux + KVM + firecracker on PATH + fetched test artifacts).
//!
//! Run via:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-sandbox-firecracker --test multi_restore -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig};

/// How many serial restore-destroy cycles to exercise. 3 is the
/// smallest N that catches "first restore works but second fails"
/// (state-bin path collisions, leaked TAP, leaked NBD slot, etc.).
const N: usize = 3;

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker; run with --ignored on the dev VM"]
async fn serial_restore_from_one_canonical_n_times() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };

    let work = tempfile::tempdir().expect("tempdir");
    let local_rootfs = work.path().join("rootfs.ext4");
    tokio::fs::copy(&env.rootfs, &local_rootfs)
        .await
        .expect("clone rootfs into tempdir");

    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = None;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "multi-restore-source".into(),
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

    // Step 1: Create source + let it settle + snapshot + destroy.
    let source_id = backend.create(spec).await.expect("create source");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let metadata = backend.snapshot(source_id).await.expect("snapshot source");
    backend.destroy(source_id).await.expect("destroy source");

    // Step 2: N serial restore→destroy cycles. This is the
    // warm-pool-refill semantic: each iteration is what happens
    // when a leased slot gets consumed and the refill loop
    // produces a fresh one. We assert each cycle is independent
    // — distinct restored SandboxIds, no leaked TAP / NBD / vsock
    // / firecracker child state across iterations.
    //
    // The test deliberately stops at "restore + verify list +
    // destroy" rather than exec'ing inside the restored VM: the
    // public ubuntu test rootfs has no engram-agentd, so vsock
    // exec wouldn't succeed regardless of the multi-restore
    // machinery being correct (same constraint the existing
    // `snapshot.rs` round-trip test works around).
    let mut seen_ids = std::collections::HashSet::new();
    for i in 0..N {
        let id = backend
            .restore(metadata.clone())
            .await
            .unwrap_or_else(|e| panic!("restore iter {i}: {e}"));
        assert!(
            seen_ids.insert(id),
            "iter {i} restored SandboxId {id} must be fresh",
        );

        let listed = backend
            .list()
            .await
            .unwrap_or_else(|e| panic!("list iter {i}: {e}"));
        assert!(
            listed.contains(&id),
            "iter {i} restored sandbox must appear in list()",
        );

        backend
            .destroy(id)
            .await
            .unwrap_or_else(|e| panic!("destroy iter {i}: {e}"));

        let listed = backend
            .list()
            .await
            .unwrap_or_else(|e| panic!("list-after-destroy iter {i}: {e}"));
        assert!(
            !listed.contains(&id),
            "iter {i} destroyed sandbox must not appear in list()",
        );
    }
}

/// Vsock UDS re-key regression: N same-snapshot restores ALIVE AT
/// ONCE on one host, each owning its own id-keyed UDS. Pre-re-key
/// this scenario was impossible (every descendant bound the source's
/// path; the last binder stole its siblings' channels).
#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker; run with --ignored on the dev VM"]
async fn concurrent_restores_from_one_snapshot_rekey_vsock() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };

    let work = tempfile::tempdir().expect("tempdir");
    let local_rootfs = work.path().join("rootfs.ext4");
    tokio::fs::copy(&env.rootfs, &local_rootfs)
        .await
        .expect("clone rootfs into tempdir");

    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = None;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "concurrent-restore-source".into(),
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

    let source_id = backend.create(spec).await.expect("create source");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let metadata = backend.snapshot(source_id).await.expect("snapshot source");
    backend.destroy(source_id).await.expect("destroy source");
    let source_vsock = work.path().join(format!("{source_id}.vsock"));

    // All N restores live simultaneously.
    let mut ids = Vec::new();
    for i in 0..N {
        let id = backend
            .restore(metadata.clone())
            .await
            .unwrap_or_else(|e| panic!("concurrent restore {i}: {e}"));
        ids.push(id);
    }
    let listed = backend.list().await.expect("list");
    for id in &ids {
        assert!(listed.contains(id), "{id} must be alive in list()");
    }

    // Each restored VM's FC must have bound its OWN id-keyed UDS —
    // a real listener, not just a leftover file: connect() succeeds
    // only against a live bind.
    for id in &ids {
        let own_uds = work.path().join(format!("{id}.vsock"));
        tokio::net::UnixStream::connect(&own_uds)
            .await
            .unwrap_or_else(|e| {
                panic!("restored {id} must own a live vsock listener at {own_uds:?}: {e}")
            });
        assert_ne!(
            own_uds, source_vsock,
            "{id} must NOT be keyed to the source path",
        );
    }

    // Destroying one sibling must not disturb another's socket (the
    // old shared-path world failed exactly here: one teardown/unlink
    // broke every descendant's channels).
    let (victim, survivors) = ids.split_first().expect("at least one restore");
    backend.destroy(*victim).await.expect("destroy victim");
    for id in survivors {
        let own_uds = work.path().join(format!("{id}.vsock"));
        tokio::net::UnixStream::connect(&own_uds)
            .await
            .unwrap_or_else(|e| {
                panic!("survivor {id} lost its vsock listener after a sibling destroy: {e}")
            });
    }
    for id in survivors {
        backend.destroy(*id).await.expect("destroy survivor");
    }
}
