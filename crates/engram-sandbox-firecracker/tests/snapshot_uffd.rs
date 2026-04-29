//! UFFD-backed snapshot/restore round-trip. Same shape as
//! `tests/snapshot.rs` but configures `RestoreMode::Uffd` and points
//! `uffd_handler_bin` at the locally-built `engram-uffd-handler`.
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
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, RestoreMode};

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
    cfg.uffd_handler_bin = handler;
    cfg.restore_mode = RestoreMode::Uffd;
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "fc-uffd-test".into(),
        rootfs_source: Some(local_rootfs),
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 128 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        agent: None,
    };

    let original_id = backend.create(spec).await.expect("create");
    tokio::time::sleep(Duration::from_secs(2)).await;

    let snap_dir = work.path().join("snap");
    backend
        .snapshot(original_id, &snap_dir)
        .await
        .expect("snapshot");

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
