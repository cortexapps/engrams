//! UFFD-backed snapshot/restore round-trip. Same shape as
//! `tests/snapshot.rs` but configures `RestoreMode::Uffd` and points
//! `uffd_handler_bin` at the locally-built `engram-uffd-handler`.
//!
//! ADR 0007: UFFD restore requires a `memory_manifest` on the
//! snapshot's FC sidecar JSON. In production the host-agent's
//! `PooledBackend::snapshot` chunks memory.bin into the chunk
//! store and patches the manifest on the operator's behalf;
//! pulling host-agent into this crate would be a circular dep,
//! so the test inlines the same chunking + patch sequence on the
//! emitted memory.bin before invoking restore. That keeps FC's
//! UFFD restore path exercised end-to-end against a real chunk
//! store + content-addressed bytes.
//!
//! Gating + run instructions match the other FC integration tests:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo build -p engram-uffd-handler
//! cargo test -p engram-sandbox-firecracker --test snapshot_uffd -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engram_chunk_store::{ChunkStore, ManifestKind};
use engram_core::traits::sandbox::SandboxBackend;
use engram_core::traits::BlobStorage;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, RestoreMode};
use engram_storage_local::LocalBlobStorage;

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + built engram-uffd-handler"]
async fn snapshot_then_uffd_restore_round_trips_microvm() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };

    // Find the built engram-uffd-handler binary. Workspace target
    // root is two `..`s up from this crate's manifest dir.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let handler = target_root.join("debug").join("engram-uffd-handler");
    if !handler.exists() {
        eprintln!(
            "SKIP: engram-uffd-handler not built at {} — run `cargo build -p engram-uffd-handler` first",
            handler.display()
        );
        return;
    }

    let work = tempfile::tempdir().expect("tempdir");
    let local_rootfs = work.path().join("rootfs.ext4");
    common::clone_rootfs(&env.rootfs, &local_rootfs)
        .await
        .expect("clone rootfs into tempdir");

    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    // Unprivileged test — see lifecycle.rs comment.
    cfg.net_pool = None;
    cfg.uffd_handler_bin = handler;
    cfg.restore_mode = RestoreMode::Uffd;
    // Public ubuntu-22.04 rootfs has no /sbin/engram-init; default
    // boot args would kernel-panic 1–2s in. Boot to bash instead so
    // the VM survives the 2s sleep before snapshot. See snapshot.rs
    // for the full rationale.
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "fc-uffd-test".into(),
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

    let original_id = backend.create(spec).await.expect("create");
    // Wait for the kernel to actually start (banner on the serial console
    // funneled to firecracker.log) rather than sleeping a fixed worst-case.
    // Fails fast on a kernel panic; returns the instant the VM is up.
    let _ = common::wait_for_log_contains(
        &work
            .path()
            .join(original_id.to_string())
            .join("firecracker.log"),
        &["Linux version"],
        Duration::from_secs(15),
    )
    .await;

    // ADR 0007 Phase 6: backend owns its staging dir.
    let metadata = backend.snapshot(original_id).await.expect("snapshot");
    let snap_dir = backend.snapshot_path_for(metadata.id);

    // ADR 0007: Inline the PooledBackend chunk-memory-on-snapshot
    // wrap. Production wires this via `PooledBackend::with_chunk_store`;
    // FC tests can't take that dep without a cycle, so reproduce
    // the on-disk effect here so the UFFD restore has the manifest
    // ref it now requires.
    //
    // The spawned handler reads `ENGRAM_LOCAL_PATH/blobs` for the
    // local-mode blob backend, so put chunks at exactly that path
    // and set the env var before invoking restore.
    let local_path = work.path().join("local-state");
    let blobs_dir = local_path.join("blobs");
    std::env::set_var("ENGRAM_BLOB_BACKEND", "local");
    std::env::set_var("ENGRAM_LOCAL_PATH", &local_path);
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(blobs_dir));
    let cs = ChunkStore::new(blob);
    let memory_bin = snap_dir.join("memory.bin");
    let manifest = cs
        .chunk_file(&memory_bin, ManifestKind::Memory, None)
        .await
        .expect("chunk memory.bin");
    let manifest_ref = ManifestRef::new();
    cs.put_manifest(manifest_ref, &manifest)
        .await
        .expect("put memory manifest");
    let manifest_json = snap_dir.join("manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&manifest_json).await.expect("read mj"))
            .expect("parse mj");
    value.as_object_mut().expect("mj root is an object").insert(
        "memory_manifest".into(),
        serde_json::to_value(manifest_ref).expect("serialize mref"),
    );
    tokio::fs::write(
        &manifest_json,
        serde_json::to_vec_pretty(&value).expect("re-serialize mj"),
    )
    .await
    .expect("write patched mj");

    backend
        .destroy(original_id)
        .await
        .expect("destroy original");

    // The whole point of UFFD: this should return well before
    // memory.bin would have been read in full. We're not benchmarking
    // here, but a > 1s budget catches a regression where the backend
    // accidentally fell back to File mode (which on this hardware
    // takes a few hundred ms — but with UFFD it should be tens).
    // Make the backend leave the jail dir behind on failure so we can
    // grab firecracker.log + uffd-handler.log for the panic message.
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    let restore_start = std::time::Instant::now();
    // Build a metadata that mirrors what the production coord
    // would persist + replay: same snapshot id, same chunked
    // memory_manifest we just wrote.
    let restore_metadata = engram_core::types::snapshot::SnapshotMetadata {
        id: metadata.id,
        size_bytes: metadata.size_bytes,
        created_at: metadata.created_at,
        image_version: metadata.image_version.clone(),
        disk_manifest: metadata.disk_manifest,
        memory_manifest: Some(manifest_ref),
        base_memory_manifest: None,
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
    let restored_id = match backend.restore(restore_metadata).await {
        Ok(id) => id,
        Err(e) => {
            // The preserved jail dir lives under work_dir; dump every
            // *.log we find so a CI failure shows the full picture.
            for entry in std::fs::read_dir(work.path())
                .into_iter()
                .flatten()
                .flatten()
            {
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
            panic!("restore: {e:?}");
        }
    };
    let restore_elapsed = restore_start.elapsed();
    assert!(
        restore_elapsed < Duration::from_secs(2),
        "UFFD restore should return promptly; took {restore_elapsed:?}",
    );

    // Restored VM is alive. The handler is paging in memory lazily;
    // for the trait contract, what matters is that `list` shows it
    // and `snapshot_state` reflects it.
    let listed = backend.list().await.expect("list after restore");
    assert_eq!(listed, vec![restored_id]);

    // Poll the handler log until it reports it's up (listening /
    // handshake) instead of a fixed sleep. If the handler crashed
    // silently the needles never appear and we fall through to the
    // assertion below with the actual log contents; otherwise we
    // proceed the instant it's serving pages.
    let _ = common::wait_for_log_contains(
        &work
            .path()
            .join(restored_id.to_string())
            .join("uffd-handler.log"),
        &["listening", "handshake"],
        Duration::from_secs(5),
    )
    .await;
    let restored_jail = work.path().join(restored_id.to_string());
    let log_path = restored_jail.join("uffd-handler.log");
    assert!(
        log_path.exists(),
        "uffd-handler log missing at {}",
        log_path.display(),
    );
    let log_content = tokio::fs::read_to_string(&log_path)
        .await
        .unwrap_or_default();
    assert!(
        log_content.contains("listening") || log_content.contains("handshake"),
        "expected handler startup messages in log, got: {log_content}",
    );

    backend
        .destroy(restored_id)
        .await
        .expect("destroy restored");
}

/// Prod regression guard, 2026-05-29. After ENGRAM_FC_RESTORE_MODE=uffd
/// became the default on FC hosts, the first cross-host UFFD restore
/// after a MIG roll failed with
/// `snapshot memory.bin missing at /var/lib/.../<id>/memory.bin`.
///
/// Root cause: `restore_in_jail` did an unconditional existence check
/// for memory.bin before the mode-specific load branch — fine in File
/// mode (where the file is required), wrong in UFFD mode (where the
/// handler serves pages from chunks and memory.bin is never read by
/// `PUT /snapshot/load`). Same-host UFFD restore had been working
/// only because `PooledBackend::snapshot` had written memory.bin
/// locally at capture; cross-host receivers correctly skip
/// `materialize_memory_if_missing` when `restore_memory_is_lazy()`
/// returns true, leaving memory.bin absent on disk.
///
/// This test reproduces the cross-host shape: take a snapshot (memory.bin
/// exists), chunk its memory into the chunk store, delete the local
/// memory.bin, then restore. With the bug the restore errors at the
/// existence check; with the fix it succeeds via the UFFD handler.
#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + built engram-uffd-handler"]
async fn uffd_restore_succeeds_when_memory_bin_absent_locally() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };

    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let handler = target_root.join("debug").join("engram-uffd-handler");
    if !handler.exists() {
        eprintln!(
            "SKIP: engram-uffd-handler not built at {} — run `cargo build -p engram-uffd-handler` first",
            handler.display()
        );
        return;
    }

    let work = tempfile::tempdir().expect("tempdir");
    let local_rootfs = work.path().join("rootfs.ext4");
    common::clone_rootfs(&env.rootfs, &local_rootfs)
        .await
        .expect("clone rootfs into tempdir");

    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = None;
    cfg.uffd_handler_bin = handler;
    cfg.restore_mode = RestoreMode::Uffd;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "fc-uffd-cross-host-test".into(),
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

    let original_id = backend.create(spec).await.expect("create");
    // Wait for the kernel to actually start (banner on the serial console
    // funneled to firecracker.log) rather than sleeping a fixed worst-case.
    // Fails fast on a kernel panic; returns the instant the VM is up.
    let _ = common::wait_for_log_contains(
        &work
            .path()
            .join(original_id.to_string())
            .join("firecracker.log"),
        &["Linux version"],
        Duration::from_secs(15),
    )
    .await;

    let metadata = backend.snapshot(original_id).await.expect("snapshot");
    let snap_dir = backend.snapshot_path_for(metadata.id);

    // Inline the PooledBackend chunk-memory-on-snapshot wrap, as in the
    // sibling test — chunks the just-emitted memory.bin into the local
    // chunk store and patches the FC sidecar's `memory_manifest`.
    let local_path = work.path().join("local-state");
    let blobs_dir = local_path.join("blobs");
    std::env::set_var("ENGRAM_BLOB_BACKEND", "local");
    std::env::set_var("ENGRAM_LOCAL_PATH", &local_path);
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(blobs_dir));
    let cs = ChunkStore::new(blob);
    let memory_bin = snap_dir.join("memory.bin");
    let chunked_manifest = cs
        .chunk_file(&memory_bin, ManifestKind::Memory, None)
        .await
        .expect("chunk memory.bin");
    let manifest_ref = ManifestRef::new();
    cs.put_manifest(manifest_ref, &chunked_manifest)
        .await
        .expect("put memory manifest");
    let manifest_json = snap_dir.join("manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&manifest_json).await.expect("read mj"))
            .expect("parse mj");
    value.as_object_mut().expect("mj root is an object").insert(
        "memory_manifest".into(),
        serde_json::to_value(manifest_ref).expect("serialize mref"),
    );
    tokio::fs::write(
        &manifest_json,
        serde_json::to_vec_pretty(&value).expect("re-serialize mj"),
    )
    .await
    .expect("write patched mj");

    backend
        .destroy(original_id)
        .await
        .expect("destroy original");

    // The cross-host simulation: delete memory.bin locally. After this
    // the snapshot dir contains state.bin + manifest.json + the
    // chunked memory manifest in the chunk store, which is exactly
    // what a cross-host receiver has after `materialize_state_if_missing`
    // runs but `materialize_memory_if_missing` is intentionally
    // skipped (lazy memory restore).
    tokio::fs::remove_file(&memory_bin)
        .await
        .expect("delete memory.bin to simulate cross-host receive");
    assert!(
        !memory_bin.exists(),
        "memory.bin should be absent for this regression case",
    );

    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    let restore_metadata = engram_core::types::snapshot::SnapshotMetadata {
        id: metadata.id,
        size_bytes: metadata.size_bytes,
        created_at: metadata.created_at,
        image_version: metadata.image_version.clone(),
        disk_manifest: metadata.disk_manifest,
        memory_manifest: Some(manifest_ref),
        base_memory_manifest: None,
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
    let restored_id = match backend.restore(restore_metadata).await {
        Ok(id) => id,
        Err(e) => {
            // The prod bug surfaced as exactly this error. Dump logs
            // and panic with a message that calls it out by name.
            for entry in std::fs::read_dir(work.path())
                .into_iter()
                .flatten()
                .flatten()
            {
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
            panic!(
                "UFFD restore without local memory.bin failed — this is the prod blocker \
                 (cross-host MIG-roll first-restore) we're guarding against: {e:?}",
            );
        }
    };

    let listed = backend.list().await.expect("list after restore");
    assert_eq!(listed, vec![restored_id]);

    backend
        .destroy(restored_id)
        .await
        .expect("destroy restored");
}
