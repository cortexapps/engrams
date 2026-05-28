//! ADR 0018 commit 12o: chained-lineage restore correctness gate.
//!
//! The re-evacuation shape that prod surfaced: a sandbox is restored
//! from a snapshot, then SNAPSHOTTED AGAIN, then restored from THAT
//! snapshot. This is distinct from `multi_restore.rs` (which restores
//! N times from ONE snapshot — the warm-pool refill shape). Here each
//! hop snapshots the *restored* sandbox, so the snapshot lineage
//! chains: cold → S1 → restore R1 → S2 → restore R2 → …
//!
//! Pre-12o this failed on the second restore. FC's `state.bin` embeds
//! absolute, sandbox-id-keyed device paths (rootfs `path_on_host` and
//! the vsock UDS); a plain `load_snapshot` reopens them at the paths
//! the snapshotting VM had. Because restore never re-points those
//! devices, a restored sandbox keeps running with its ANCESTOR's
//! id-keyed paths, while `snapshot()` recomputed
//! `source_rootfs_canonical` / `source_vsock_canonical` from the live
//! id. After hop 1 the two diverged, so `restore_canonical_symlinks`
//! recreated/cleaned the (computed) live-id paths while FC's
//! `load_snapshot` opened the (embedded) ancestor paths that nobody
//! touched → ENOENT on rootfs ("Block: Virtio backend error: ... No
//! such file or directory rootfs/<ancestor>.dev") and, once rootfs was
//! fixed in isolation, EADDRINUSE on the vsock UDS. Traced against
//! prod snapshot artifacts.
//!
//! 12o fixes it (Option A) by stamping `source_rootfs_canonical` and
//! `source_vsock_canonical` from the paths the LIVE sandbox actually
//! has open (tracked in `SandboxState`), not recomputed from the live
//! id. A snapshot lineage is one logical machine with stable device
//! paths; the per-restore sandbox_id is just a routing handle. This is
//! the only viable fix for vsock (FC refuses PUT /vsock post-load), and
//! rootfs uses the same mechanism for uniformity. This test is the
//! regression gate: it must get through at least two chained restores
//! across BOTH devices.
//!
//! Flat-file rootfs (not NBD) is sufficient to reproduce — the bug
//! lives in the canonical-path stamping layer
//! (`source_{rootfs,vsock}_canonical`), identical for flat-file and
//! NBD-backed rootfs.
//!
//! ADR 0021 P1.5 retired the harness virtio-blk drive (the harness
//! lives in the rootfs now), so §12p's harness-drive lineage gate
//! doesn't apply anymore — there's nothing for `restore` to
//! re-anchor. The test still exercises the rootfs + vsock canonical
//! paths (the §12o territory), which is the regression that
//! originally motivated this file.
//!
//! Same gating as the other ignored FC integration tests (Linux +
//! KVM + firecracker on PATH + fetched test artifacts).
//!
//! Run via:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-sandbox-firecracker --test restore_chain -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig};

/// Number of chained restore→snapshot hops past the cold-create
/// snapshot. 2 is the smallest that catches the 12o regression: hop 1
/// restores a cold snapshot (always worked), hop 2 restores a
/// snapshot-of-a-restored-sandbox (the broken case). 3 adds margin
/// that the fix holds across a deeper chain (no slow accumulation of
/// stale symlinks / drifting embedded paths).
const HOPS: usize = 3;

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker; run with --ignored on the dev VM"]
async fn chained_restore_snapshot_lineage_holds() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };

    let work = tempfile::tempdir().expect("tempdir");
    let local_rootfs = work.path().join("rootfs.ext4");
    tokio::fs::copy(&env.rootfs, &local_rootfs)
        .await
        .expect("clone rootfs into tempdir");

    // ADR 0021 P1.5: no harness substrate — the harness lives in
    // the rootfs at the manifest-declared `[harness] exec` path,
    // and there's no separate virtio-blk drive to anchor across
    // restores.

    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = None;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "restore-chain-source".into(),
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

    // Cold create + settle + snapshot S1 + destroy. `metadata` is the
    // snapshot we restore the first hop from.
    let source_id = backend.create(spec).await.expect("create source");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut metadata = backend.snapshot(source_id).await.expect("snapshot source");
    backend.destroy(source_id).await.expect("destroy source");

    // Chained hops. Each hop:
    //   1. restore from the previous snapshot,
    //   2. snapshot the RESTORED sandbox (the chaining step — this is
    //      what produces a snapshot whose state.bin embeds a restored
    //      sandbox's id-keyed rootfs path),
    //   3. destroy the restored sandbox,
    //   4. carry the new snapshot forward to the next hop.
    //
    // Hop 1 restores the cold snapshot (the case that always worked).
    // Hop 2+ restore a snapshot-of-a-restored-sandbox — the 12o
    // regression. A restore failure here surfaces as the panic on
    // `restore`.
    let mut seen = std::collections::HashSet::new();
    seen.insert(source_id);
    for hop in 1..=HOPS {
        let restored = backend
            .restore(metadata.clone())
            .await
            .unwrap_or_else(|e| panic!("hop {hop}: restore failed: {e}"));
        assert!(
            seen.insert(restored),
            "hop {hop}: restored SandboxId {restored} must be fresh",
        );
        let listed = backend
            .list()
            .await
            .unwrap_or_else(|e| panic!("hop {hop}: list: {e}"));
        assert!(
            listed.contains(&restored),
            "hop {hop}: restored sandbox {restored} must appear in list()",
        );

        // Let the restored VM settle, then snapshot IT — this is the
        // step that chains the lineage. Pre-12o, the snapshot here
        // recorded source_{rootfs,vsock}_canonical from `restored`'s id
        // while state.bin still embedded the ancestor's paths; the NEXT
        // hop's restore then couldn't find the embedded rootfs path
        // (ENOENT) / collided on the embedded vsock UDS (EADDRINUSE).
        // With 12o the snapshot stamps the paths `restored` actually
        // has open (the ancestor's, carried forward via SandboxState),
        // so restore_canonical_symlinks recreates/cleans exactly those
        // and the next restore resolves cleanly for both devices.
        //
        // §12p adds the third device: the harness drive. `restore`
        // re-anchors it onto the live id (PATCH /drives) so the embedded
        // path tracks the live id and this snapshot's recomputed
        // source_harness_canonical matches what the next hop's
        // load_snapshot opens — no ENOENT on harness/<ancestor>.ext4.
        tokio::time::sleep(Duration::from_secs(2)).await;
        metadata = backend
            .snapshot(restored)
            .await
            .unwrap_or_else(|e| panic!("hop {hop}: snapshot of restored sandbox: {e}"));

        backend
            .destroy(restored)
            .await
            .unwrap_or_else(|e| panic!("hop {hop}: destroy: {e}"));
    }
}
