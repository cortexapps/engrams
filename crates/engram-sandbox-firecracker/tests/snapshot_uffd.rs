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
    tokio::fs::copy(&env.rootfs, &local_rootfs)
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
        harness_pack_uri: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 128 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        harness_substrate: None,
        network: Default::default(),
        canonical_memory_manifest: None,
    };

    let original_id = backend.create(spec).await.expect("create");
    tokio::time::sleep(Duration::from_secs(2)).await;

    let snap_dir = work.path().join("snap");
    backend
        .snapshot(original_id, &snap_dir)
        .await
        .expect("snapshot");

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
    let restored_id = match backend.restore(snap_dir.clone()).await {
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

    // Give the kernel a moment to fault in some pages. If the
    // handler crashed silently we'd see those faults stall the VM
    // and a destroy would still pass. To check the handler is
    // alive we read the log — non-empty is good enough for v1.
    tokio::time::sleep(Duration::from_secs(1)).await;
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
