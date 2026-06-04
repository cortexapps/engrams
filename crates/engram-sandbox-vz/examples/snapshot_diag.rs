//! Snapshot/restore diagnostic for the VZ backend.
//!
//! Boots a minimal VM (no agentd, no harness), idles for a few seconds,
//! pauses, saves to disk, releases the VM, then constructs a fresh VM
//! and tries to restore. Surfaces the real `NSError` code+domain on
//! failure so we can match it against `VZErrorCode`.
//!
//! Three scenarios, controlled by env:
//!
//! ```text
//!   ENGRAM_DIAG_SCENARIO=base       — kernel + rootfs only, no virtio-console multi-port
//!   ENGRAM_DIAG_SCENARIO=console    — adds 3-port virtio-console (current production config)
//!   ENGRAM_DIAG_SCENARIO=cold-restore — does the save+restore cycle on a kernel-boot-only VM
//! ```
//!
//! Required env:
//!   ENGRAM_VZ_KERNEL_PATH   = arm64 vmlinux
//!   ENGRAM_VZ_ROOTFS_PATH   = arm64 ext4 rootfs (any of our baked images)
//!   ENGRAM_VZ_SCRATCH       = scratch dir for the state.bin
//!
//! Run: `cargo run -p engram-sandbox-vz --example snapshot_diag`
//! (codesign first — `just vz-codesign`).

#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::time::Duration;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("snapshot_diag is macOS-only");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() {
    real_main().await;
}

#[cfg(target_os = "macos")]
async fn real_main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .with_writer(std::io::stderr)
        .init();
    let kernel = std::env::var("ENGRAM_VZ_KERNEL_PATH").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/.cache/engram-vz-test/vmlinux-arm64")
    });
    let rootfs = std::env::var("ENGRAM_VZ_ROOTFS_PATH")
        .unwrap_or_else(|_| "./var/engram/images/local:/demo/warm-1/rootfs.ext4".into());
    let scratch = std::env::var("ENGRAM_VZ_SCRATCH")
        .unwrap_or_else(|_| "/tmp/engram-vz-snapshot-diag".into());
    let scenario = std::env::var("ENGRAM_DIAG_SCENARIO").unwrap_or_else(|_| "base".into());

    eprintln!("== snapshot_diag ==");
    eprintln!("  kernel:   {kernel}");
    eprintln!("  rootfs:   {rootfs}");
    eprintln!("  scratch:  {scratch}");
    eprintln!("  scenario: {scenario}");
    eprintln!();

    let _ = std::fs::create_dir_all(&scratch);
    let state_path = PathBuf::from(&scratch).join("state.bin");
    let _ = std::fs::remove_file(&state_path);

    match scenario.as_str() {
        "base" => run_scenario(&kernel, &rootfs, &state_path, false).await,
        "console" => run_scenario(&kernel, &rootfs, &state_path, true).await,
        "cold-restore" => run_cold_restore(&kernel, &rootfs, &state_path).await,
        other => {
            eprintln!("unknown scenario: {other}");
            std::process::exit(2);
        }
    }
}

// On macOS we can use the crate's internals via integration-test
// access. The crate's lib only exposes VzBackend publicly today;
// for this diagnostic we use VzBackend::create + snapshot + restore
// directly, which exercises the same code path the coord uses.

#[cfg(target_os = "macos")]
async fn run_scenario(
    kernel: &str,
    rootfs: &str,
    // ADR 0007 Phase 6: backend owns its staging dir; the caller-
    // provided `state_path` is no longer load-bearing (kept in the
    // signature so the existing CLI dispatch in main doesn't have
    // to change).
    _state_path: &std::path::Path,
    _with_console: bool,
) {
    use engram_core::traits::SandboxBackend;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
    use std::collections::HashMap;

    let cfg = engram_sandbox_vz::VzConfig::with_kernel(kernel);
    let work_dir = "/tmp/engram-vz-diag-work";
    let _ = std::fs::create_dir_all(work_dir);
    let backend = match engram_sandbox_vz::VzBackend::new(work_dir, cfg) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("VzBackend::new failed: {e}");
            std::process::exit(1);
        }
    };

    let spec = SandboxSpec {
        image: "diag".into(),
        rootfs_source: Some(PathBuf::from(rootfs)),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 512 },
        disk: DiskLimit { max_gib: 10 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    };

    eprintln!("[diag] backend.create — booting VM");
    let id = match backend.create(spec).await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("create failed: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[diag] sandbox_id={id}; sleeping 5s for boot to settle");
    tokio::time::sleep(Duration::from_secs(5)).await;

    // ADR 0007 Phase 6: backend owns its staging dir. Capture the
    // metadata after the snapshot completes so we can look up the
    // dir for the post-snapshot manifest rewrite below.
    eprintln!("[diag] backend.snapshot (Phase 6: backend-owned staging)");
    let snap_metadata = match backend.snapshot(id).await {
        Ok(meta) => {
            eprintln!("[diag] snapshot ok: size={} bytes", meta.size_bytes);
            meta
        }
        Err(e) => {
            eprintln!("[diag] snapshot failed: {e}");
            std::process::exit(1);
        }
    };
    let snap_dir = backend.snapshot_path_for(snap_metadata.id);
    eprintln!("[diag] snapshot dir = {}", snap_dir.display());

    // ENGRAM_DIAG_FROZEN_DISK=1 → copy the rootfs IMMEDIATELY after
    // save and rewrite the snapshot manifest to point at the copy.
    // Tests the "VZ rejects restore because the disk has mutated
    // since save" hypothesis. APFS clonefile on a 1.7 GB ext4 is
    // ~50ms, so the copy is well under the post-save resume gap
    // and the rootfs we run from is bytewise stable.
    if std::env::var("ENGRAM_DIAG_FROZEN_DISK").as_deref() == Ok("1") {
        let frozen = snap_dir.join("rootfs.ext4");
        eprintln!("[diag] freezing rootfs → {}", frozen.display());
        // SAFETY: fresh process; clonefile(2) is sound when source +
        // destination paths are valid C strings on the same APFS
        // volume.
        let src_c = std::ffi::CString::new(rootfs).unwrap();
        let dst_c = std::ffi::CString::new(frozen.to_string_lossy().as_bytes()).unwrap();
        let _ = std::fs::remove_file(&frozen);
        let r = unsafe { libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
        if r != 0 {
            eprintln!(
                "[diag] clonefile failed: {} — falling back to fs::copy",
                std::io::Error::last_os_error()
            );
            std::fs::copy(rootfs, &frozen).expect("fs::copy fallback");
        }
        // Rewrite manifest.json to reference the frozen rootfs.
        let manifest_path = snap_dir.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["spec"]["rootfs_source"] =
            serde_json::Value::String(frozen.to_string_lossy().into());
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        eprintln!("[diag] manifest rewritten to use frozen rootfs");
    }

    if std::env::var("ENGRAM_DIAG_RESTORE_MODE").as_deref() == Ok("fresh") {
        eprintln!("[diag] destroying source VM");
        let _ = backend.destroy(id).await;
        drop(backend);

        eprintln!("[diag] reconstructing fresh VzBackend; calling restore");
        let cfg = engram_sandbox_vz::VzConfig::with_kernel(kernel);
        let backend = engram_sandbox_vz::VzBackend::new(work_dir, cfg).expect("backend rebuild");
        match backend.restore(snap_metadata.clone()).await {
            Ok(new_id) => {
                eprintln!("[diag] restore ok! new_id={new_id}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                let _ = backend.destroy(new_id).await;
            }
            Err(e) => {
                eprintln!("[diag] restore FAILED: {e}");
                std::process::exit(1);
            }
        }
    } else {
        eprintln!("[diag] same-instance restore: destroying then restoring on same backend");
        let _ = backend.destroy(id).await;
        match backend.restore(snap_metadata).await {
            Ok(new_id) => {
                eprintln!("[diag] same-instance restore ok! new_id={new_id}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                let _ = backend.destroy(new_id).await;
            }
            Err(e) => {
                eprintln!("[diag] same-instance restore FAILED: {e}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(target_os = "macos")]
async fn run_cold_restore(kernel: &str, rootfs: &str, state_path: &std::path::Path) {
    // Same as run_scenario("base") for now — placeholder for a future
    // variant that drops the virtio-console device entirely. Today
    // VzBackend always attaches one, so we'd need a separate code
    // path to skip it.
    run_scenario(kernel, rootfs, state_path, false).await;
}
