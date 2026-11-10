//! ADR 0045 unified memory substrate (v2b) — end-to-end over real FC.
//!
//! Same shape as `tests/snapshot.rs`, but with `uffd_base_dir` set:
//! the restore sends `uffd_base_file` to the FORKED Firecracker, which
//! maps guest memory `MAP_PRIVATE` on the per-template base shm file and
//! registers UFFD `MISSING|MINOR`; the handler populates the base with
//! canonical chunks and resolves faults via `UFFDIO_CONTINUE` (shared)
//! instead of `UFFDIO_COPY` (private).
//!
//! Asserts:
//!   - the restore round-trips and the restored VM is alive
//!   - the base shm file exists at the derived path, is sized to the
//!     manifest, and is PARTIALLY POPULATED (the handler pwrote canonical
//!     chunks into it — `SEEK_DATA` finds extents)
//!   - a SECOND sibling restored from the same snapshot works against the
//!     same (already-populated) base file
//!
//! Density (PSS) parity is the ADR 0045 D3/D4 gate, measured separately —
//! this test is the D2 correctness gate.
//!
//! Needs the FORKED firecracker (`uffd_base_file` is a fork-only load
//! param; stock FC `deny_unknown_fields`-rejects it): set
//! `ENGRAM_FC_FORK_BIN`, as for `stock_fork_snapshot_compat`. Skips
//! cleanly when unset.
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo build -p engram-uffd-handler
//! export ENGRAM_FC_FORK_BIN=/path/to/forked/firecracker
//! cargo test -p engram-sandbox-firecracker --test substrate_uffd_base -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use engram_chunk_store::ManifestKind;
use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::ids::SandboxId;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig};

/// First data extent at-or-after `offset`, or `None` if all-hole from
/// there. Used to prove the handler actually populated the base file.
fn first_data_at(file: &std::fs::File, offset: u64) -> Option<u64> {
    // SAFETY: plain lseek on an owned fd.
    let r = unsafe { libc::lseek64(file.as_raw_fd(), offset as libc::off64_t, libc::SEEK_DATA) };
    (r >= 0).then_some(r as u64)
}

#[tokio::test]
#[ignore = "requires Linux + KVM + ENGRAM_FC_FORK_BIN + built engram-uffd-handler"]
async fn substrate_base_shm_restore_round_trips_and_shares() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };
    let fork_bin = match std::env::var("ENGRAM_FC_FORK_BIN") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => {
            eprintln!("SKIP: ENGRAM_FC_FORK_BIN not set (uffd_base_file is a fork-only param)");
            return;
        }
    };

    // The base dir MUST be tmpfs/shmem (UFFD MINOR is shmem-only).
    let base_dir = PathBuf::from(format!(
        "/dev/shm/engram-substrate-test-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&base_dir).expect("create base dir on /dev/shm");

    let work = tempfile::tempdir().expect("tempdir");
    let local_rootfs = work.path().join("rootfs.ext4");
    common::clone_rootfs(&env.rootfs, &local_rootfs)
        .await
        .expect("clone rootfs into tempdir");

    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = None;
    cfg.firecracker_bin = fork_bin;
    common::resume_through_uffd(&mut cfg, work.path());
    cfg.uffd_base_dir = Some(base_dir.clone());
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "fc-substrate-test".into(),
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
        swap_source: None,
        swap_manifest: None,
    };

    let original_id = SandboxId::new();
    backend.create(original_id, spec).await.expect("create");
    // Fixed settle before snapshot. The substrate tests re-capture VMs restored
    // from this snapshot, and a too-early (kernel-banner) snapshot leaves the
    // restored VM too fragile to re-snapshot (FC socket gone). Let early boot
    // finish — there's no cheap host signal for "settled enough to re-snapshot".
    tokio::time::sleep(Duration::from_secs(2)).await;
    let metadata = backend.snapshot(original_id).await.expect("snapshot");
    let snap_dir = backend.snapshot_path_for(metadata.id);

    let manifest_ref = common::chunk_memory(&snap_dir, work.path()).await;
    let cs = common::uffd_chunk_store(work.path());

    backend
        .destroy(original_id)
        .await
        .expect("destroy original");
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    let restore_metadata = engram_core::types::snapshot::SnapshotMetadata {
        id: metadata.id,
        size_bytes: metadata.size_bytes,
        created_at: metadata.created_at,
        image_version: metadata.image_version.clone(),
        swap_manifest: metadata.swap_manifest,
        disk_manifest: metadata.disk_manifest,
        memory_manifest: Some(manifest_ref),
        base_memory_manifest: None,
        base_disk_manifest: None,
        migration_source: None,
        source_sandbox_id: metadata.source_sandbox_id,
        state_blob_key: metadata.state_blob_key.clone(),
        sidecar_blob_key: metadata.sidecar_blob_key.clone(),
        rootfs_blob_key: metadata.rootfs_blob_key.clone(),
        working_set_blob_key: metadata.working_set_blob_key.clone(),
        aux_bundles: metadata.aux_bundles.clone(),
        paused_at: metadata.paused_at,
        peer_hints: Vec::new(),
    };

    let dump_logs_and_panic = |what: &str, e: String, work_path: &Path| -> ! {
        for entry in std::fs::read_dir(work_path).into_iter().flatten().flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            for log_name in &["firecracker.log", "uffd-handler.log"] {
                let log = entry.path().join(log_name);
                if log.exists() {
                    let content = std::fs::read_to_string(&log).unwrap_or_default();
                    eprintln!("--- {} ---\n{content}", log.display());
                }
            }
        }
        panic!("{what}: {e}");
    };

    let restored_id = SandboxId::new();
    match backend
        .restore(
            restored_id,
            restore_metadata.clone(),
            common::disks(&local_rootfs),
        )
        .await
    {
        Ok(()) => {}
        Err(e) => dump_logs_and_panic("restore #1", format!("{e:?}"), work.path()),
    };

    // The substrate's derived base path: <dir>/<manifest>-v<version>.base.
    let base_path = base_dir.join(format!(
        "{}-v{}.base",
        manifest_ref.manifest_id, manifest_ref.version
    ));
    assert!(
        base_path.exists(),
        "handler should have created the base shm at {}",
        base_path.display()
    );
    let base_file = std::fs::File::open(&base_path).expect("open base shm");
    assert_eq!(
        base_file.metadata().expect("stat base").len(),
        128 * 1024 * 1024,
        "base shm sized to guest memory"
    );

    // Wait for the guest to fault pages through the handler, then prove the
    // handler populated the base (canonical chunks pwritten — the shared-install
    // path, not private COPYs). Poll the SEEK_DATA predicate instead of sleeping;
    // the assertion below stays as the failure signal.
    common::poll_until(Duration::from_secs(10), Duration::from_millis(200), || {
        first_data_at(&base_file, 0).is_some()
    })
    .await;
    assert!(
        first_data_at(&base_file, 0).is_some(),
        "base shm has no data extents — canonical faults did not go through \
         the shared install path"
    );

    // Handler log sanity: substrate mode announced itself. Poll the handler log
    // for the startup line rather than reading it once.
    let restored_jail = work.path().join(restored_id.to_string());
    let log_content = common::wait_for_log_contains(
        &restored_jail.join("uffd-handler.log"),
        &["substrate base shm ready"],
        Duration::from_secs(10),
    )
    .await;
    assert!(
        log_content.contains("substrate base shm ready"),
        "expected substrate-mode startup line in handler log, got: {log_content}"
    );

    // Sibling restore from the same snapshot: exercises the
    // already-populated base (SEEK_HOLE fast path + shared CONTINUE).
    let sibling_id = SandboxId::new();
    match backend
        .restore(
            sibling_id,
            restore_metadata.clone(),
            common::disks(&local_rootfs),
        )
        .await
    {
        Ok(()) => {}
        Err(e) => dump_logs_and_panic("restore #2 (sibling)", format!("{e:?}"), work.path()),
    };
    // Poll list() until both siblings appear rather than sleeping a fixed window;
    // the assertion below stays as the failure signal.
    common::poll_until_async(
        Duration::from_secs(5),
        Duration::from_millis(200),
        || async {
            let mut listed = backend.list().await.expect("list");
            listed.sort();
            let mut expect = vec![restored_id, sibling_id];
            expect.sort();
            listed == expect
        },
    )
    .await;
    let mut listed = backend.list().await.expect("list");
    listed.sort();
    let mut expect = vec![restored_id, sibling_id];
    expect.sort();
    assert_eq!(listed, expect, "both substrate-backed VMs alive");

    // An image base whose layout does not match the session (the image was
    // re-captured after the session's snapshot): the handler falls back to a
    // restore without a shared base and never creates the image base file.
    // The load must not name that file, or Firecracker fails with ENOENT.
    let short_memory = work.path().join("short-memory.bin");
    std::fs::write(&short_memory, vec![0u8; 4 * 1024 * 1024]).expect("write short memory");
    let short_chunked = cs
        .chunk_file(&short_memory, ManifestKind::Memory, None)
        .await
        .expect("chunk short memory");
    let mismatched_base = ManifestRef::new();
    cs.put_manifest(mismatched_base, &short_chunked)
        .await
        .expect("put mismatched base manifest");
    let mut mismatched_metadata = restore_metadata;
    mismatched_metadata.base_memory_manifest = Some(mismatched_base);
    let fallback_id = SandboxId::new();
    match backend
        .restore(
            fallback_id,
            mismatched_metadata,
            common::disks(&local_rootfs),
        )
        .await
    {
        Ok(()) => {}
        Err(e) => dump_logs_and_panic(
            "restore #3 (mismatched base)",
            format!("{e:?}"),
            work.path(),
        ),
    };
    let mismatched_base_path = base_dir.join(format!(
        "{}-v{}.base",
        mismatched_base.manifest_id, mismatched_base.version
    ));
    assert!(
        !mismatched_base_path.exists(),
        "the fallback restore must not create the mismatched image base"
    );
    backend
        .destroy(fallback_id)
        .await
        .expect("destroy fallback");

    backend.destroy(sibling_id).await.expect("destroy sibling");
    backend
        .destroy(restored_id)
        .await
        .expect("destroy restored");
    let _ = std::fs::remove_dir_all(&base_dir);
}

/// ADR 0045 D3 durability gate: **capture OF a substrate-backed VM** —
/// the eviction/checkpoint direction, which every other test runs the
/// other way around (capture a normal VM, restore onto the substrate).
///
/// Restores a VM onto the substrate, lets the guest run (dirty pages are
/// kernel-native COW over the MAP_PRIVATE base), then:
///   1. takes a FULL snapshot of the substrate VM and restores a new VM
///      from THAT snapshot (again on the substrate) — it must come up
///      alive (a corrupted capture wedges the guest immediately);
///   2. takes a DIFF snapshot (KVM dirty-log over the MAP_PRIVATE
///      backing, `track_dirty_pages`) and asserts the sparse
///      memory.diff exists and is much smaller than guest RAM — the
///      dirty bitmap must see exactly the COW writes, not the whole
///      shared base.
///
/// The full diff-CHAIN (PooledBackend sparse re-chunk + chained
/// restore) is exercised by the dev-stack lifecycle smoke; this gates
/// the FC-level capture mechanics in CI.
#[tokio::test]
#[ignore = "requires Linux + KVM + ENGRAM_FC_FORK_BIN + built engram-uffd-handler"]
async fn capture_of_substrate_vm_round_trips_and_diffs() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };
    let fork_bin = match std::env::var("ENGRAM_FC_FORK_BIN") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => {
            eprintln!("SKIP: ENGRAM_FC_FORK_BIN not set (substrate is fork-only)");
            return;
        }
    };
    let base_dir = PathBuf::from(format!(
        "/dev/shm/engram-substrate-capture-test-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&base_dir).expect("create base dir on /dev/shm");

    let work = tempfile::tempdir().expect("tempdir");
    let local_rootfs = work.path().join("rootfs.ext4");
    common::clone_rootfs(&env.rootfs, &local_rootfs)
        .await
        .expect("clone rootfs into tempdir");

    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = None;
    cfg.firecracker_bin = fork_bin;
    common::resume_through_uffd(&mut cfg, work.path());
    cfg.uffd_base_dir = Some(base_dir.clone());
    // Arm KVM dirty tracking so the Diff capture of the substrate VM works
    // (and so the substrate load passes enable_diff_snapshots, matching
    // prod's checkpoint-chain configuration).
    cfg.track_dirty_pages = true;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "fc-substrate-capture-test".into(),
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
        swap_source: None,
        swap_manifest: None,
    };

    // Source VM -> snapshot -> chunked manifest (the substrate's source).
    let original_id = SandboxId::new();
    backend.create(original_id, spec).await.expect("create");
    // Fixed settle before snapshot. The substrate tests re-capture VMs restored
    // from this snapshot, and a too-early (kernel-banner) snapshot leaves the
    // restored VM too fragile to re-snapshot (FC socket gone). Let early boot
    // finish — there's no cheap host signal for "settled enough to re-snapshot".
    tokio::time::sleep(Duration::from_secs(2)).await;
    let metadata = backend.snapshot(original_id).await.expect("snapshot");
    let snap_dir = backend.snapshot_path_for(metadata.id);

    let manifest_ref = common::chunk_memory(&snap_dir, work.path()).await;
    backend
        .destroy(original_id)
        .await
        .expect("destroy original");
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    let mut restore_metadata = metadata.clone();
    restore_metadata.memory_manifest = Some(manifest_ref);

    // Restore #1 onto the substrate; let the guest run + COW-dirty pages.
    let substrate_vm = SandboxId::new();
    backend
        .restore(
            substrate_vm,
            restore_metadata.clone(),
            common::disks(&local_rootfs),
        )
        .await
        .expect("restore #1 onto the substrate");
    // Irreducible open-loop churn window (NOT a readiness wait): let the guest
    // COW-dirty pages over the MAP_PRIVATE base. There is no host-observable lower
    // bound to poll toward — the downstream diff-size check is an UPPER bound that
    // even zero churn satisfies, so this stays a fixed sleep.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 1. FULL capture OF the substrate VM (the eviction direction).
    let capture = backend
        .snapshot(substrate_vm)
        .await
        .expect("FULL capture of the substrate-backed VM");
    let cap_dir = backend.snapshot_path_for(capture.id);
    let cap_mem = cap_dir.join("memory.bin");
    let cap_len = std::fs::metadata(&cap_mem)
        .expect("captured memory.bin")
        .len();
    assert_eq!(
        cap_len,
        128 * 1024 * 1024,
        "full capture of a substrate VM must dump the complete guest RAM"
    );

    // 2. DIFF capture: the KVM dirty log over the MAP_PRIVATE backing must
    //    see only the COW writes since the Full capture reset the bitmap.
    // Irreducible open-loop churn window (NOT a readiness wait): nothing
    // host-observable to poll toward; the diff-size assertion is an upper bound
    // that even zero churn satisfies, so this stays a fixed sleep.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let diff = backend
        .snapshot_diff(substrate_vm)
        .await
        .expect("DIFF capture of the substrate-backed VM");
    let diff_mem = backend.snapshot_path_for(diff.id).join("memory.diff");
    let diff_meta = std::fs::metadata(&diff_mem).expect("captured memory.diff");
    // Sparse file: apparent size == guest RAM, allocated blocks == dirty set.
    let allocated = std::os::unix::fs::MetadataExt::blocks(&diff_meta) * 512;
    eprintln!(
        "SPIKE: diff-of-substrate allocated {} KiB of {} KiB guest RAM",
        allocated / 1024,
        diff_meta.len() / 1024
    );
    assert!(
        allocated < 64 * 1024 * 1024,
        "diff capture of a substrate VM allocated {} bytes — dirty tracking \
         is seeing the shared base, not just the COW writes",
        allocated
    );

    // 3. Restore #2 FROM the capture-of-substrate snapshot (chunk it like
    //    the eviction pipeline would) — must come up alive.
    let manifest_ref2 = common::chunk_memory(&cap_dir, work.path()).await;
    let mut meta2 = capture.clone();
    meta2.memory_manifest = Some(manifest_ref2);
    // ADR 0045 D4: the canonical split — pass the IMAGE's base manifest so
    // the revival CONTINUEs base-identical pages against the SHARED
    // per-image base file instead of minting a session-keyed one.
    meta2.base_memory_manifest = Some(manifest_ref);

    let revived = SandboxId::new();
    backend
        .restore(revived, meta2, common::disks(&local_rootfs))
        .await
        .expect("restore FROM the capture-of-substrate snapshot");
    // Poll list() until both VMs appear rather than sleeping a fixed window; the
    // assertion below stays as the failure signal.
    common::poll_until_async(
        Duration::from_secs(5),
        Duration::from_millis(200),
        || async {
            let mut listed = backend.list().await.expect("list");
            listed.sort();
            let mut expect = vec![substrate_vm, revived];
            expect.sort();
            listed == expect
        },
    )
    .await;
    let mut listed = backend.list().await.expect("list");
    listed.sort();
    let mut expect = vec![substrate_vm, revived];
    expect.sort();
    assert_eq!(
        listed, expect,
        "both the substrate VM and its revived capture are alive"
    );

    // D4 assertion: exactly ONE base file — the revival shared the image
    // base (derived from base_memory_manifest) rather than creating a
    // second file keyed by its own session manifest.
    let base_files: Vec<_> = std::fs::read_dir(&base_dir)
        .expect("read base dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        base_files.len(),
        1,
        "canonical split must reuse the per-image base file, got: {base_files:?}"
    );
    assert!(
        base_files[0].starts_with(&manifest_ref.manifest_id.to_string()),
        "the single base file is the image base: {base_files:?}"
    );

    backend.destroy(revived).await.expect("destroy revived");
    backend
        .destroy(substrate_vm)
        .await
        .expect("destroy substrate vm");
    let _ = std::fs::remove_dir_all(&base_dir);
}
