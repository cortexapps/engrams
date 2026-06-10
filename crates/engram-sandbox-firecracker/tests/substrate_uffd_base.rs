//! ADR 0045 unified memory substrate (v2b) — end-to-end over real FC.
//!
//! Same shape as `tests/snapshot_uffd.rs`, but with `uffd_base_dir` set:
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
use std::sync::Arc;
use std::time::Duration;

use engram_chunk_store::{ChunkStore, ManifestKind};
use engram_core::traits::sandbox::SandboxBackend;
use engram_core::traits::BlobStorage;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, RestoreMode};
use engram_storage_local::LocalBlobStorage;

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

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest_dir)
        .join("..")
        .join("..")
        .join("target");
    let handler = target_root.join("debug").join("engram-uffd-handler");
    if !handler.exists() {
        eprintln!(
            "SKIP: engram-uffd-handler not built at {} — run `cargo build -p engram-uffd-handler`",
            handler.display()
        );
        return;
    }

    // The base dir MUST be tmpfs/shmem (UFFD MINOR is shmem-only).
    let base_dir = PathBuf::from(format!(
        "/dev/shm/engram-substrate-test-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&base_dir).expect("create base dir on /dev/shm");

    let work = tempfile::tempdir().expect("tempdir");
    let local_rootfs = work.path().join("rootfs.ext4");
    tokio::fs::copy(&env.rootfs, &local_rootfs)
        .await
        .expect("clone rootfs into tempdir");

    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = None;
    cfg.firecracker_bin = fork_bin;
    cfg.uffd_handler_bin = handler;
    cfg.restore_mode = RestoreMode::Uffd;
    cfg.uffd_base_dir = Some(base_dir.clone());
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "fc-substrate-test".into(),
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
    };

    let original_id = backend.create(spec).await.expect("create");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let metadata = backend.snapshot(original_id).await.expect("snapshot");
    let snap_dir = backend.snapshot_path_for(metadata.id);

    // Inline the PooledBackend memory-chunking wrap (see snapshot_uffd.rs
    // for the full rationale — a host-agent dep here would be circular).
    let local_path = work.path().join("local-state");
    let blobs_dir = local_path.join("blobs");
    std::env::set_var("ENGRAM_BLOB_BACKEND", "local");
    std::env::set_var("ENGRAM_LOCAL_PATH", &local_path);
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(blobs_dir));
    let cs = ChunkStore::new(blob);
    let memory_bin = snap_dir.join("memory.bin");
    let chunked = cs
        .chunk_file(&memory_bin, ManifestKind::Memory, None)
        .await
        .expect("chunk memory.bin");
    let manifest_ref = ManifestRef::new();
    cs.put_manifest(manifest_ref, &chunked)
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
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    let restore_metadata = engram_core::types::snapshot::SnapshotMetadata {
        id: metadata.id,
        size_bytes: metadata.size_bytes,
        created_at: metadata.created_at,
        image_version: metadata.image_version.clone(),
        disk_manifest: metadata.disk_manifest,
        memory_manifest: Some(manifest_ref),
        source_sandbox_id: metadata.source_sandbox_id,
        state_blob_key: metadata.state_blob_key.clone(),
        sidecar_blob_key: metadata.sidecar_blob_key.clone(),
        rootfs_blob_key: metadata.rootfs_blob_key.clone(),
        working_set_blob_key: metadata.working_set_blob_key.clone(),
        aux_bundles: metadata.aux_bundles.clone(),
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

    let restored_id = match backend.restore(restore_metadata.clone()).await {
        Ok(id) => id,
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

    // Give the guest a moment to fault pages through the handler, then
    // prove the handler populated the base (canonical chunks pwritten —
    // the shared-install path, not private COPYs).
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        first_data_at(&base_file, 0).is_some(),
        "base shm has no data extents — canonical faults did not go through \
         the shared install path"
    );

    // Handler log sanity: substrate mode announced itself.
    let restored_jail = work.path().join(restored_id.to_string());
    let log_content =
        std::fs::read_to_string(restored_jail.join("uffd-handler.log")).unwrap_or_default();
    assert!(
        log_content.contains("substrate base shm ready"),
        "expected substrate-mode startup line in handler log, got: {log_content}"
    );

    // Sibling restore from the same snapshot: exercises the
    // already-populated base (SEEK_HOLE fast path + shared CONTINUE).
    let sibling_id = match backend.restore(restore_metadata).await {
        Ok(id) => id,
        Err(e) => dump_logs_and_panic("restore #2 (sibling)", format!("{e:?}"), work.path()),
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut listed = backend.list().await.expect("list");
    listed.sort();
    let mut expect = vec![restored_id, sibling_id];
    expect.sort();
    assert_eq!(listed, expect, "both substrate-backed VMs alive");

    backend.destroy(sibling_id).await.expect("destroy sibling");
    backend
        .destroy(restored_id)
        .await
        .expect("destroy restored");
    let _ = std::fs::remove_dir_all(&base_dir);
}
